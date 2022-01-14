use fuzzer_options::parse_args;

use std::env;
use std::process;
use std::process::abort;

use libafl::bolts::current_nanos;
use libafl::bolts::launcher::Launcher;
use libafl::bolts::rands::StdRand;
use libafl::bolts::shmem::{ShMemProvider, StdShMemProvider};
use libafl::bolts::tuples::{tuple_list, Merge};
use libafl::corpus::ondisk::OnDiskMetadataFormat;
use libafl::corpus::{
    Corpus, IndexesLenTimeMinimizerCorpusScheduler, OnDiskCorpus, QueueCorpusScheduler,
};
use libafl::events::EventConfig;
use libafl::executors::{ExitKind, TimeoutExecutor};
use libafl::feedbacks::{CrashFeedback, MapFeedbackState, MaxMapFeedback, TimeFeedback};
use libafl::fuzzer::{Fuzzer, StdFuzzer};
use libafl::inputs::{BytesInput, HasTargetBytes};
use libafl::monitors::MultiMonitor;
use libafl::mutators::{havoc_mutations, tokens_mutations, StdScheduledMutator, Tokens};
use libafl::observers::{HitcountsMapObserver, ObserversTuple, TimeObserver, VariableMapObserver};
use libafl::stages::StdMutationalStage;
use libafl::state::{HasCorpus, HasMetadata, StdState};
use libafl::{feedback_and_fast, feedback_or, Error};

use libafl_qemu::elf::EasyElf;
use libafl_qemu::{
    edges, Emulator, MmapPerms, QemuAsanHelper, QemuExecutor, QemuHelper, QemuHelperTuple,
    SYS_exit, SYS_exit_group, SYS_mmap, SYS_munmap, SYS_read, SyscallHookResult,
};

/// maximum size allowed for our mmap'd input file
const MMAP_SIZE: usize = 1048576; // 2**20 | 1 MB

/// wrapper around buffer generated from BytesInput that can be accessed from within the syscall
/// hooking function / saves us from using something like lazy_static for passing information from
/// the harness to the syscall hook and vice-versa
#[derive(Default, Debug)]
struct QemuFilesystemBytesHelper {
    /// initially, the buffer generated by calling BytesInput.target_bytes(). on successive calls
    /// to `SYS_read`, the vector will shrink as bytes are passed from this buffer to the buffer
    /// specified in the syscall
    bytes: Vec<u8>,

    /// address we'll return for calls to mmap
    mmap_addr: u64,
}

impl QemuFilesystemBytesHelper {
    /// given an address to use as the address for an mmap'd file, create a new
    /// QemuFilesystemBytesHelper  
    fn new(mmap_addr: u64) -> Self {
        Self {
            mmap_addr,
            ..Default::default()
        }
    }
}

/// implement the QemuHelper trait for QemuFilesystemBytesHelper
impl<S> QemuHelper<BytesInput, S> for QemuFilesystemBytesHelper
where
    S: HasMetadata,
{
    /// initialize QemuFilesystemBytesHelper; only called on fuzzer spawns/respawns
    fn init<'a, H, OT, QT>(&self, executor: &QemuExecutor<'a, H, BytesInput, OT, QT, S>)
    where
        H: FnMut(&BytesInput) -> ExitKind,
        OT: ObserversTuple<BytesInput, S>,
        QT: QemuHelperTuple<BytesInput, S>,
    {
        // for every `QemuHelper` passed to `QemuExecutor` via a `QemuHelperTuple`,
        // `QemuHelper::init` is called by `QemuExecutor::new`. we'll use the call to init to pass
        // our syscall hook into `QemuExecutor::hook_syscalls`, which is the proper place to pass
        // our hook. The hooks on the Emulator are 'raw' hooks, and not what we're looking for in
        // this particular case
        executor.hook_syscalls(syscall_hook::<QT, S>);
    }

    /// prepare helper for fuzz case; called before every fuzz case
    fn pre_exec(&mut self, _emulator: &Emulator, input: &BytesInput) {
        // similar to `QemuHelper::init`, `QemuHelper::pre_exec` is called via a `QemuHelperTuple`
        // and each `QemuHelper` in the tuple can expect to have pre_exec called. The flow for
        // `QemuExecutor` is (basically) `pre_exec_all` => `run_target` => `post_exec_all`. We'll
        // use the pre_exec call to perform what would normally be placed at the beginning of the
        // harness code. We'll save off the buffer and its length for use later in the syscall
        // hook.
        let target = input.target_bytes();
        let mut buf = target.as_slice();

        if buf.len() > MMAP_SIZE {
            buf = &buf[0..MMAP_SIZE];
        }

        self.bytes.clear();
        self.bytes.extend_from_slice(buf);
    }
}

/// wrapper around general purpose register resets, mimics AFL_QEMU_PERSISTENT_GPR
///   ref: https://github.com/AFLplusplus/AFLplusplus/blob/stable/qemu_mode/README.persistent.md#24-resetting-the-register-state
#[derive(Default, Debug)]
struct QemuGPRegisterHelper {
    /// vector of values representing each registers saved value
    register_state: Vec<u64>,
}

/// implement the QemuHelper trait for QemuGPRegisterHelper
impl<S> QemuHelper<BytesInput, S> for QemuGPRegisterHelper
where
    S: HasMetadata,
{
    /// prepare helper for fuzz case; called before every fuzz case
    fn pre_exec(&mut self, emulator: &Emulator, _input: &BytesInput) {
        self.restore(emulator);
    }
}

/// QemuGPRegisterHelper implementation
impl QemuGPRegisterHelper {
    /// given an `Emulator`, save off all known register values
    fn new(emulator: &Emulator) -> Self {
        let register_state = (0..emulator.num_regs())
            .map(|reg_idx| emulator.read_reg(reg_idx).unwrap_or(0))
            .collect::<Vec<u64>>();

        Self { register_state }
    }

    /// restore emulator's registers to previously saved values
    fn restore(&self, emulator: &Emulator) {
        self.register_state
            .iter()
            .enumerate()
            .for_each(|(reg_idx, reg_val)| {
                if let Err(e) = emulator.write_reg(reg_idx as i32, *reg_val) {
                    println!(
                        "[ERR] Couldn't set register x{} ({}), skipping...",
                        reg_idx, e
                    )
                }
            })
    }
}

/// from man syscall
///   arch/ABI    instruction           syscall #  retval
///   arm64       svc #0                x8         x0
///
///   arch/ABI      arg1  arg2  arg3  arg4  arg5  arg6  arg7
///   arm64         x0    x1    x2    x3    x4    x5    -
///
/// hook signature where ... are add'l u64's
///   fn(&Emulator, &mut QT, &mut S, sys_num: i32, u64, ...) -> SyscallHookResult
#[allow(clippy::too_many_arguments)]
fn syscall_hook<QT, S>(
    emulator: &Emulator, // our instantiated Emulator
    helpers: &mut QT,    // QemuFilesystemBytesHelper is passed in here, among others
    _state: &mut S,
    syscall: i32, // syscall number
    x0: u64,      // registers ...
    x1: u64,
    x2: u64,
    _: u64,
    _: u64,
    _: u64,
    _: u64,
    _: u64,
) -> SyscallHookResult
where
    QT: QemuHelperTuple<BytesInput, S>,
{
    let syscall = syscall as i64;

    if syscall == SYS_mmap {
        // man mmap
        //
        //   void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset);
        //   The address of the new mapping is returned as the result of the call.
        let fs_helper = helpers
            .match_first_type_mut::<QemuFilesystemBytesHelper>()
            .unwrap();

        SyscallHookResult::new(Some(fs_helper.mmap_addr))
    } else if syscall == SYS_munmap {
        // man munmap
        //
        //   int munmap(void *addr, size_t length);
        //   On success, munmap() returns 0.  On failure, it returns -1, and errno is set
        let fs_helper = helpers
            .match_first_type_mut::<QemuFilesystemBytesHelper>()
            .unwrap();

        if x0 == fs_helper.mmap_addr {
            // munmap call to our managed memory location, return success but do nothing else
            SyscallHookResult::new(Some(0))
        } else {
            // let it ride
            SyscallHookResult::new(None)
        }
    } else if syscall == SYS_read {
        // man read:
        //
        //   ssize_t read(int fd, void *buf, size_t count);
        //
        //   On  success, the number of bytes read is returned (zero indicates end of file)
        //   On error, -1 is returned, and errno is set appropriately.
        let fs_helper = helpers
            .match_first_type_mut::<QemuFilesystemBytesHelper>()
            .unwrap();

        let current_len = fs_helper.bytes.len();

        let offset: usize = if x2 == 0 {
            // ask for nothing, get nothing
            0
        } else if x2 as usize <= current_len {
            // normal non-negative read that's less than the current mutated buffer's total
            // length
            x2.try_into().unwrap()
        } else {
            // length requested is more than what our buffer holds, so we can read up to the
            // end of the buffer
            current_len
        };

        // draining iterator that removes the specified range from the vector
        // and returns the removed items.
        //
        // when the iterator is dropped, all elements in the range are removed
        // from the vector
        let drained = fs_helper.bytes.drain(..offset);

        unsafe {
            // write the requested number of bytes to the buffer sent to the read syscall
            emulator.write_mem(x1, drained.as_slice());
        }

        SyscallHookResult::new(Some(drained.len() as u64))
        // Drain<u8> dropped here, our buffer now has only what remains of the original u8's
    } else if syscall == SYS_exit || syscall == SYS_exit_group {
        // since calls to exit are verboten, hook the relevant syscalls and abort instead
        abort();
    } else {
        SyscallHookResult::new(None) // all other syscalls pass through unaltered
    }
}

fn main() -> Result<(), Error> {
    // parse the following:
    //   solutions dir
    //   input corpus dirs
    //   cores
    //   timeout
    //   verbosity
    //   broker port
    //   stdout file
    //   token files
    let fuzzer_options = parse_args();

    //
    // Component: Corpus
    //

    // path to input corpus directory
    let corpus_dirs = fuzzer_options.corpora;

    // corpus that will be evolved in memory, during fuzzing; metadata saved in json
    let input_corpus = OnDiskCorpus::new_save_meta(
        fuzzer_options.crashes.join("queue"),
        Some(OnDiskMetadataFormat::JsonPretty),
    )?;

    // corpus in which we store solutions on disk so we can get them after stopping the fuzzer
    let solutions_corpus = OnDiskCorpus::new(fuzzer_options.crashes)?;

    //
    // Component: Emulator
    //

    env::remove_var("LD_LIBRARY_PATH");

    let mut args: Vec<String> = env::args().collect();
    let mut env: Vec<(String, String)> = env::vars().collect();

    // create an Emulator which provides the methods necessary to interact with the emulated target
    let emu = libafl_qemu::init_with_asan(&mut args, &mut env);

    // load our fuzz target from disk, the resulting `EasyElf` is used to do symbol lookups on the
    // binary. It handles address resolution in the case of PIE as well.
    let mut buffer = Vec::new();
    let elf = EasyElf::from_file(emu.binary_path(), &mut buffer)?;

    // find the function of interest from the loaded elf. since we're not interested in parsing
    // command line stuff every time, we'll run until main, and then set our entrypoint to be past
    // the getopt stuff by adding a static offset found by looking at the disassembly. This is the
    // same concept as using AFL_ENTRYPOINT.
    let main_ptr = elf.resolve_symbol("main", emu.load_addr()).unwrap();

    // in main, after arg parsing, before call to TIFFOpen
    let adjusted_main_ptr = main_ptr + 0x178;

    // point at which we want to stop execution, i.e. before return from main and before optind++
    let ret_addr = main_ptr + 0x144;

    // set a breakpoint on the function of interest and emulate execution until we arrive there
    emu.set_breakpoint(adjusted_main_ptr);
    unsafe { emu.run() };

    // reset breakpoint from start of the function to the place we want to stop, registers will
    // all be saved off in `QemuGPRegisterHelper::pre_exec`
    emu.remove_breakpoint(adjusted_main_ptr);
    emu.set_breakpoint(ret_addr);

    // reserve space for our `BytesInput` in memory. allows us to manage it during `mmap` calls
    let input_addr = emu.map_private(0, MMAP_SIZE, MmapPerms::ReadWrite).unwrap();

    //
    // Component: Harness
    //

    let mut harness = |_: &BytesInput| {
        // the `BytesInput` value is taken care of by the QemuFilesystemBytesHelper during pre_exec
        // and the syscall hook, so there's no need to do anything with it here.
        //
        // additionally, resetting registers to a known good state is handled by
        // QemuGPRegisterHelper during its pre_exec, so all we have to do is call .run
        unsafe {
            emu.run();
        };

        ExitKind::Ok
    };

    //
    // Component: Client Runner
    //

    let mut run_client = |state: Option<_>, mut mgr, _core_id| {
        //
        // Component: Observer
        //

        // Create an observation channel using the coverage map.
        //
        // the `libafl_qemu::edges` module re-exports the same `EDGES_MAP` and `MAX_EDGES_NUM`
        // from `libafl_targets`, meaning we're using the sancov backend for coverage
        let edges = unsafe { &mut edges::EDGES_MAP };
        let edges_size = unsafe { &mut edges::MAX_EDGES_NUM };
        let edges_observer =
            HitcountsMapObserver::new(VariableMapObserver::new("edges", edges, edges_size));

        // Create an observation channel to keep track of the execution time and previous runtime
        let time_observer = TimeObserver::new("time");

        //
        // Component: Feedback
        //

        // This is the state of the data that the feedback wants to persist in the fuzzer's state. In
        // particular, it is the cumulative map holding all the edges seen so far that is used to track
        // edge coverage.
        let feedback_state = MapFeedbackState::with_observer(&edges_observer);

        // A Feedback, in most cases, processes the information reported by one or more observers to
        // decide if the execution is interesting. This one is composed of two Feedbacks using a
        // logical OR.
        //
        // Due to the fact that TimeFeedback can never classify a testcase as interesting on its own,
        // we need to use it alongside some other Feedback that has the ability to perform said
        // classification. These two feedbacks are combined to create a boolean formula, i.e. if the
        // input triggered a new code path, OR, false.
        let feedback = feedback_or!(
            // New maximization map feedback (attempts to maximize the map contents) linked to the
            // edges observer and the feedback state. This one will track indexes, but will not track
            // novelties, i.e. new_tracking(... true, false).
            MaxMapFeedback::new_tracking(&feedback_state, &edges_observer, true, false),
            // Time feedback, this one does not need a feedback state, nor does it ever return true for
            // is_interesting, However, it does keep track of testcase execution time by way of its
            // TimeObserver
            TimeFeedback::new_with_observer(&time_observer)
        );

        // A feedback, when used as an Objective, determines if an input should be added to the
        // corpus or not. In the case below, we're saying that in order for a testcase's input to
        // be added to the corpus, it must:
        //
        //   1: be a crash
        //        AND
        //   2: have produced new edge coverage
        //
        // The feedback_and_fast macro combines the two feedbacks with a fast AND operation, which
        // means only enough feedback functions will be called to know whether or not the objective
        // has been met, i.e. short-circuiting logic.
        //
        // this is essentially the same crash deduplication strategy used by afl++
        let objective_state = MapFeedbackState::new("dedup_edges", edges::EDGES_MAP_SIZE);
        let objective = feedback_and_fast!(
            CrashFeedback::new(),
            MaxMapFeedback::new(&objective_state, &edges_observer)
        );

        //
        // Component: State
        //

        // Creates a new State, taking ownership of all of the individual components during fuzzing.
        //
        // On the initial pass, state will be None, and the `unwrap_or_else` will populate our
        // initial settings.
        //
        // On each successive execution, the state from the prior run that was saved
        // off in shared memory will be passed into the closure. The code below handles the
        // initial None value by providing a default StdState. After the first restart, we'll
        // simply unwrap the Some(StdState) passed in to the closure
        let mut state = state.unwrap_or_else(|| {
            StdState::new(
                // random number generator with a time-based seed
                StdRand::with_seed(current_nanos()),
                // input corpus
                input_corpus.clone(),
                // solutions corpus
                solutions_corpus.clone(),
                // States of the feedbacks that store the data related to the feedbacks that should be
                // persisted in the State.
                tuple_list!(feedback_state, objective_state),
            )
        });

        // populate tokens metadata from token files, if provided. Tokens are the LibAFL term for
        // what AFL et. al. call a Dictionary
        if state.metadata().get::<Tokens>().is_none() && !fuzzer_options.token_files.is_empty() {
            // metadata hasn't been populated with tokens yet, and we have token files that should
            // be read; populate the metadata from each token file
            for token_file in &fuzzer_options.token_files {
                state.add_metadata(Tokens::from_tokens_file(token_file)?);
            }
        }

        //
        // Component: Scheduler
        //

        // A minimization + queue policy to get test cases from the corpus
        //
        // IndexesLenTimeMinimizerCorpusScheduler is a MinimizerCorpusScheduler with a
        // LenTimeMulFavFactor that prioritizes quick and small Testcases that exercise all the
        // entries registered in the MapIndexesMetadata
        //
        // a QueueCorpusScheduler walks the corpus in a queue-like fashion
        let scheduler = IndexesLenTimeMinimizerCorpusScheduler::new(QueueCorpusScheduler::new());

        //
        // Component: Fuzzer
        //

        // A fuzzer with feedback, objectives, and a corpus scheduler
        let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

        //
        // Component: Executor
        //

        // Create an in-process executor backed by QEMU. The QemuExecutor wraps the
        // `InProcessExecutor`, all of the `QemuHelper`s and the `Emulator` (in addition to the
        // normal wrapped components). This gives us an executor that will execute a bunch of testcases
        // within the same process, eliminating a lot of the overhead associated with a fork/exec or
        // forkserver execution model.
        //
        // additionally, each of the helpers and the emulator will be accessible at other points
        // of execution, easing emulator/input interaction/modification
        let executor = QemuExecutor::new(
            &mut harness,
            &emu,
            tuple_list!(
                edges::QemuEdgeCoverageHelper::new(),
                QemuFilesystemBytesHelper::new(input_addr),
                QemuGPRegisterHelper::new(&emu),
                QemuAsanHelper::new(),
            ),
            tuple_list!(edges_observer, time_observer),
            &mut fuzzer,
            &mut state,
            &mut mgr,
        )?;

        // wrap the `QemuExecutor` with a `TimeoutExecutor` that sets a timeout before each run
        let mut executor = TimeoutExecutor::new(executor, fuzzer_options.timeout);

        // In case the corpus is empty (i.e. on first run), load existing test cases from on-disk
        // corpus
        if state.corpus().count() < 1 {
            state
                .load_initial_inputs(&mut fuzzer, &mut executor, &mut mgr, &corpus_dirs)
                .unwrap_or_else(|_| {
                    println!("Failed to load initial corpus at {:?}", &corpus_dirs);
                    process::exit(0);
                });
        }

        //
        // Component: Mutator
        //

        // Setup a havoc mutator and dictionary (token) mutator with a mutational stage
        let mutator = StdScheduledMutator::new(havoc_mutations().merge(tokens_mutations()));

        //
        // Component: Stage
        //

        let mut stages = tuple_list!(StdMutationalStage::new(mutator));

        fuzzer.fuzz_loop(&mut stages, &mut executor, &mut state, &mut mgr)?;

        Ok(())
    };

    //
    // Component: Monitor
    //

    let monitor = MultiMonitor::new(|s| println!("{}", s));

    // Build and run a Launcher
    match Launcher::builder()
        .shmem_provider(StdShMemProvider::new()?)
        .broker_port(fuzzer_options.port)
        .configuration(EventConfig::from_build_id())
        .monitor(monitor)
        .run_client(&mut run_client)
        .cores(&fuzzer_options.cores)
        .stdout_file(fuzzer_options.stdout.as_deref())
        .build()
        .launch()
    {
        Ok(()) => Ok(()),
        Err(Error::ShuttingDown) => {
            println!("Fuzzing stopped by user. Good bye.");
            Ok(())
        }
        Err(err) => panic!("Failed to run launcher: {:?}", err),
    }
}