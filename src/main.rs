mod callbacks;

use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::sync::mpsc::{self, Receiver, RecvError, Sender};

use chrono::Local;
use pulse::error::PAErr;
use std::io::Write;

use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, trace};
use pulse::{
    callbacks::ListResult,
    context::{
        subscribe::{Facility, InterestMaskSet, Operation},
        Context, FlagSet, State,
    },
    mainloop::signal::{Event, MainloopSignals},
    mainloop::threaded::Mainloop,
    proplist::Proplist,
};

type Sources = HashMap<u32, SourceDatum>;

type CBTX = Sender<CallbackComms>;
type CBRX = Receiver<CallbackComms>;

#[derive(Parser, Debug)]
#[clap(author = "Sam Martin-Brown", version, about)]
/// Application configuration
struct Args {
    /// whether to be verbose
    #[arg(short = 'v')]
    verbose: bool,

    /// an optional name to greet
    #[arg()]
    name: Option<String>,
}

#[derive(Debug, Clone)]
struct SourceDatum {
    name: String,
    mute: bool,
}
impl SourceDatum {
    fn new(name: String, mute: bool) -> Self {
        SourceDatum {
            name: name.to_string(),
            mute,
        }
    }
}

#[derive(Debug)]
enum Errors {
    Shutdown,
    SrcListError,
    ContextError(String),
    PAError(PAErr),
    RecvError(RecvError),
}

impl Display for Errors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Errors::Shutdown => write!(f, "Shutting down"),
            Errors::SrcListError => write!(f, "Error receiving sources from pulseaudio"),
            Errors::ContextError(context) => write!(f, "Context error: {}", context),
            Errors::PAError(pa_err) => write!(f, "PAError: {}", pa_err),
            Errors::RecvError(recv_err) => write!(f, "RecvError: {}", recv_err),
        }
    }
}
impl Error for Errors {}

impl From<PAErr> for Errors {
    fn from(value: PAErr) -> Self {
        Self::PAError(value)
    }
}

impl From<RecvError> for Errors {
    fn from(value: RecvError) -> Self {
        Self::RecvError(value)
    }
}

#[derive(Debug, Clone)]
enum SrcListState {
    // InProg,
    Item(u32, SourceDatum),
    Done,
    Err,
}

#[derive(Debug, Clone)]
enum PulseChange {
    SourceChange(u32),
    SourceNew(u32),
    SourceDrop(u32),
    Server,
}

#[derive(Debug, Clone)]
enum CallbackComms {
    Shutdown,
    CallbackDone(bool),
    ChangeType(PulseChange),
}

#[derive(Debug, Clone)]
struct ListenerState {
    // Use Pulseaudio's source index as key to source data (which is just name and mute-status)
    sources: Sources,
    default_source_id: Option<u32>,
}

impl ListenerState {
    fn new(mainloop: &mut Mainloop, context: &mut Context) -> Result<Self, Errors> {
        let sources = get_sources(context, mainloop)?;
        let default_source_id = get_default_source_index(mainloop, context, &sources)?;
        Ok(Self {
            sources,
            default_source_id,
        })
    }

    fn default_source<'a>(&'a self) -> Option<&'a SourceDatum> {
        if let Some(src_id) = self.default_source_id {
            return self.sources.get(&src_id);
        };
        None
    }
}

fn bind_signals(
    mainloop: &mut Mainloop,
    sig_tx: Sender<CallbackComms>,
) -> Result<Vec<Event>, Errors> {
    let mut signals = vec![];
    for sig_id in &[1, 2, 15] {
        let sig_tx = sig_tx.clone();

        signals.push(Event::new(*sig_id, move |sig_num| {
            // TODO: can I translate from i32 to human-readable name..?
            info!("Received a signal, num {}", sig_num);
            sig_tx.send(CallbackComms::Shutdown).unwrap();
        }));
        trace!("configuring signal handler for {}", sig_id);
    }

    mainloop.init_signals()?;
    Ok(signals)
}

fn main() -> Result<(), Errors> {
    setup_logs();

    let (tx, rx) = mpsc::channel();
    let mut mainloop =
        Mainloop::new().ok_or(Errors::ContextError("mainloop new failed".to_string()))?;
    let _sig_events = bind_signals(&mut mainloop, tx.clone())?;

    let proplist = Proplist::new().ok_or(Errors::ContextError("proplist failed".to_string()))?;
    let mut context = Context::new_with_proplist(&mainloop, "source-listener", &proplist).ok_or(
        Errors::ContextError("context::new_with_proplist failed".to_string()),
    )?;

    info!("Connecting to daemon");
    connect_to_server(&mut context, &mut mainloop, tx.clone(), &rx)?;

    let state = ListenerState::new(&mut mainloop, &mut context)?;
    report_mute_change(&state, None);
    let subscribe_result =
        subscribe_source_mute(&mut mainloop, &mut context, state, tx.clone(), rx);
    info!("shutting down");
    terminate(mainloop, context, _sig_events);

    if let Err(Errors::Shutdown) = subscribe_result {
        return Ok(());
    }
    return subscribe_result;
}

fn terminate(mut mainloop: Mainloop, mut context: Context, sig_events: Vec<Event>) {
    trace!("Disconnecting context");
    mainloop.lock();
    context.disconnect();
    mainloop.unlock();
    trace!("Stopping mainloop");
    mainloop.stop();
    trace!("dropping signal handlers");
    drop(sig_events);
    // Seems to cause crashes... unsure why
    // mainloop.signals_done();
    trace!("Termination complete");
}

fn get_source_by_idx(
    idx: u32,
    context: &Context,
    mainloop: &mut Mainloop,
) -> Result<Option<SourceDatum>, Errors> {
    // Lock mainloop to block pulseaudio from calling things during setup
    mainloop.lock();

    let introspector = context.introspect();

    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        introspector.get_source_info_by_index(idx, handle_list_result(tx));
    }

    // Unlock mainloop to let pulseaudio call the above callback.
    mainloop.unlock();
    let mut source = None;
    loop {
        let event = rx.recv()?;

        match event {
            SrcListState::Item(_, src) => {
                trace!("retrieved source info ('{}': {})", src.name, src.mute);
                source = Some(src);
            }
            SrcListState::Done => {
                return Ok(source);
            }
            SrcListState::Err => {
                info!("error retrieving source by id for {}.", &idx);
                return Err(Errors::SrcListError);
            }
        }
    }
}

fn get_sources(context: &Context, mainloop: &mut Mainloop) -> Result<Sources, Errors> {
    // Lock mainloop to block pulseaudio from calling things during setup
    mainloop.lock();

    let introspector = context.introspect();
    let (tx, rx) = mpsc::channel();

    {
        let tx = tx.clone();
        introspector.get_source_info_list(handle_list_result(tx));
    }

    let mut sources = HashMap::new();

    // Unlock mainloop to let pulseaudio call the above callback.
    mainloop.unlock();
    loop {
        let event = rx.recv()?;

        match event {
            SrcListState::Item(index, source) => {
                sources.insert(index, source);
            }
            SrcListState::Done => {
                trace!("Retrieved source info");
                return Ok(sources);
            }
            SrcListState::Err => {
                error!("error retrieving sources.");
                return Err(Errors::SrcListError);
            }
        }
    }
}

fn handle_list_result(
    tx: Sender<SrcListState>,
) -> impl Fn(ListResult<&pulse::context::introspect::SourceInfo<'_>>) {
    move |src| match src {
        ListResult::Error => {
            info!("Failed to retrieve ListResult");
            tx.send(SrcListState::Err).unwrap();
        }
        ListResult::End => {
            tx.send(SrcListState::Done).unwrap();
        }
        ListResult::Item(item) => {
            let source_name = match &item.name {
                None => "unknown".to_string(),
                Some(name) => name.to_string(),
            };

            tx.send(SrcListState::Item(
                item.index,
                SourceDatum::new(source_name, item.mute),
            ))
            .unwrap();
        }
    }
}

fn find_default_source_name(
    context: &mut Context,
    mainloop: &mut Mainloop,
) -> Result<Option<String>, Errors> {
    // Block pulseaudio from inboking callbacks
    mainloop.lock();

    let introspector = context.introspect();
    let (tx, rx) = mpsc::channel();

    {
        let tx = tx.clone();
        introspector.get_server_info(move |server_info| {
            trace!("Server info: {:?}", server_info);
            match &server_info.default_source_name {
                None => {
                    info!("no default source");
                    tx.send(None).unwrap()
                }
                Some(value) => {
                    info!("Default source: '{:?}'", value);
                    tx.send(Some(value.to_string())).unwrap();
                }
            };
        });
    }

    // Allow pulseaudio to process callbacks again
    mainloop.unlock();
    loop {
        trace!("grabbing default source value");
        let event = rx.recv()?;
        match event {
            None => {
                return Ok(None);
            }
            Some(name) => {
                return Ok(Some(name.to_owned()));
            }
        };
    }
}

fn get_default_source_index(
    mainloop: &mut Mainloop,
    context: &mut Context,
    sources: &Sources,
) -> Result<Option<u32>, Errors> {
    let default_source = find_default_source_name(context, mainloop)?;

    if let Some(default_src_name) = default_source {
        for (index, source) in sources {
            if source.name == default_src_name {
                debug!("Default source is: '{}', index: {}", source.name, index);
                return Ok(Some(*index));
            }
        }
    }

    info!("no default source available");
    Ok(None)
}

fn setup_logs() {
    let args = Args::parse();
    let log_env = if args.verbose {
        Env::default().default_filter_or("debug")
    } else {
        Env::default().default_filter_or("info")
    };
    env_logger::Builder::from_env(log_env)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}:{}:{}] ({}): {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S%.6f%z"),
                std::thread::current()
                    .name()
                    .unwrap_or(&format!("{:?}", std::thread::current().id())),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.level(),
                record.args(),
            )
        })
        .init();
}

fn subscribe_source_mute(
    mainloop: &mut Mainloop,
    context: &mut Context,
    mut state: ListenerState,
    tx: CBTX,
    rx: CBRX,
) -> Result<(), Errors> {
    // Sources toggle their mute state, default source changes Server state
    let source_mask = InterestMaskSet::SOURCE | InterestMaskSet::SERVER;

    trace!("Configuring context subscriber");

    // Block pulseaudio from invoking callbacks
    mainloop.lock();

    // tell pulseaudio to notify us about Source & Server changes
    {
        // set callback that reacts to subscription changes
        let tx = tx.clone();
        context.set_subscribe_callback(Some(Box::new(
            move |facility: Option<Facility>, operation: Option<Operation>, idx| {
                let facility = facility.unwrap();
                let operation = operation.unwrap();
                debug!(
                    "Subcribe callback: {:?}, {:?}, {:?}",
                    facility, operation, idx
                );

                match facility {
                    Facility::Source => {
                        match operation {
                            Operation::Changed => {
                                // tell callback that mainloop should update sources (can't do that here since
                                // we're already inside a callback).
                                tx.send(CallbackComms::ChangeType(PulseChange::SourceChange(idx)))
                                    .unwrap();
                            }
                            Operation::New => {
                                tx.send(CallbackComms::ChangeType(PulseChange::SourceNew(idx)))
                                    .unwrap();
                            }
                            Operation::Removed => {
                                tx.send(CallbackComms::ChangeType(PulseChange::SourceDrop(idx)))
                                    .unwrap();
                            }
                        }
                    }
                    Facility::Server => {
                        let _ = tx.send(CallbackComms::ChangeType(PulseChange::Server));
                    }
                    _ => debug!("Unrelated event: {:?}", facility),
                }
            },
        )));
    }

    context.subscribe(source_mask, |sub_success| {
        debug!(
            "Subscribing to source changes {}",
            match sub_success {
                true => "succeeded",
                false => "failed",
            }
        );
    });

    trace!("Starting subscribe mainloop");
    // Allow pulseaudio to process callbacks again
    mainloop.unlock();
    loop {
        // When we receive data via channel here, it means, we should update sources, and then
        // print if the mute state of the default source, changed.

        let old_default_mute = {
            match state.default_source() {
                Some(src) => Some(src.mute),
                None => None,
            }
        };

        let event = rx.recv()?;
        match event {
            CallbackComms::Shutdown => {
                return Err(Errors::Shutdown);
            }
            CallbackComms::ChangeType(change) => {
                // let _lock = cb_lock.lock();
                match change {
                    PulseChange::Server => {
                        debug!("Updating default source after server config change");
                        state.default_source_id =
                            get_default_source_index(mainloop, context, &state.sources)?;

                        if let Some(src) = state.default_source() {
                            info!("Default source is now: {}", src.name);
                        }
                        // Always check source changes, to ensure the new default's mute state is
                        // compared against prior mute state.
                        state.sources = get_sources(context, mainloop)?;
                    }
                    PulseChange::SourceNew(_) => {
                        // Do nothing, seems reliable that you get a change as well as a New when
                        // new devices are added, so just debounce the new's to save cpu.
                    }
                    PulseChange::SourceChange(idx) => {
                        let updated_source = match get_source_by_idx(idx, context, mainloop) {
                            Ok(res) => res,
                            Err(err) => match err {
                                Errors::SrcListError => {
                                    info!("failed to retrieve source {}, has it gone?", idx);
                                    continue;
                                }
                                _ => return Err(err),
                            },
                        };
                        match updated_source {
                            Some(src) => {
                                state.sources.insert(idx, src);

                                // If there's no current default source, see if the recent change
                                // lets us resolve one...
                                if state.default_source_id == None {
                                    state.default_source_id = get_default_source_index(
                                        mainloop,
                                        context,
                                        &state.sources,
                                    )?;
                                }
                            }
                            None => {
                                info!("failed to retrieve updated source details for src {}", &idx);
                                return Err(Errors::SrcListError);
                            }
                        }
                    }
                    PulseChange::SourceDrop(idx) => {
                        let old_src = state.sources.remove(&idx);
                        match old_src {
                            None => {
                                info!(
                                    "Tried to drop source at idx {} but it was already missing",
                                    &idx,
                                );
                            }
                            Some(src) => {
                                trace!("Removing source {} from state ({})", &idx, &src.name);
                            }
                        }
                    }
                }
            }
            _ => panic!("impossible state {:?}", event),
        }

        report_mute_change(&state, old_default_mute);
    }
}

fn report_mute_change(state: &ListenerState, old_default_mute: Option<bool>) {
    if let Some(new_src) = state.default_source() {
        if Some(new_src.mute) != old_default_mute {
            println!(
                "{}",
                match new_src.mute {
                    true => "MUTED",
                    false => "UNMUTED",
                }
            );
        }
    } else {
        println!("No default source");
    }
}

fn connect_to_server(
    context: &mut Context,
    mainloop: &mut Mainloop,
    tx: CBTX,
    rx: &CBRX,
) -> Result<(), Errors> {
    trace!("Calling context.connect");
    mainloop.lock();

    {
        // Context state boxed-callback setup
        trace!("Registering context state callback");
        context.set_state_callback(Some(Box::new(move || {
            trace!("context state changed");
            tx.send(CallbackComms::CallbackDone(true)).unwrap();
        })));
    }

    context.connect(None, FlagSet::NOAUTOSPAWN, None)?;

    mainloop.unlock();
    mainloop.start()?;

    loop {
        let event = rx.recv()?; // Wait for signal from callback.
        match event {
            CallbackComms::CallbackDone(_) => {
                // Continue once callback is received.
            }
            CallbackComms::Shutdown => {
                return Err(Errors::Shutdown);
            }
            _ => panic!("impossible state {:?}", event),
        }

        let state = context.get_state();
        match state {
            State::Unconnected | State::Connecting | State::Authorizing | State::SettingName => {
                debug!("Context state: {:?}", state);
                continue; // Use channel for synchronisation
            }
            State::Ready => {
                debug!("Context state: {:?}", state);
                break;
            }
            State::Failed => {
                debug!("Context state: {:?}", state);
                return Err(Errors::ContextError("Context Failed".into()));
            }
            State::Terminated => {
                debug!("Context state: {:?}", state);
                return Err(Errors::ContextError("Context terminated".into()));
            }
        }
    }
    // Once connected, we don't care anymore...
    context.set_state_callback(None);

    Ok(())
}
