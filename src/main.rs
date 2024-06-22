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
enum DefaultSourceState {
    NoDefault,
    Default(String),
}

#[derive(Debug, Clone)]
enum CallbackComms {
    Shutdown,
    CallbackDone(bool),
    SrcElem(SrcListState),
    ChangeType(Facility),
    ServerInfo(DefaultSourceState),
}

#[derive(Debug, Clone)]
struct ListenerState {
    // Use Pulseaudio's source index as key to source data (which is just name and mute-status)
    sources: Sources,
    default_source: u32,
}

impl ListenerState {
    fn new(
        mainloop: &mut Mainloop,
        context: &mut Context,
        tx: CBTX,
        rx: &CBRX,
    ) -> Result<Self, Errors> {
        let sources = get_sources(context, mainloop, tx.clone(), &rx)?;
        let default_source =
            get_default_source_index(mainloop, context, &sources, tx.clone(), &rx)?;
        Ok(Self {
            sources,
            default_source,
        })
    }

    fn default_source<'a>(&'a self) -> Option<&'a SourceDatum> {
        self.sources.get(&self.default_source)
    }
}

fn setup_unix_signals(
    mainloop: &mut Mainloop,
    sig_tx: Sender<CallbackComms>,
) -> Result<Vec<Event>, Errors> {
    let mut signals = vec![];
    for sig_id in [2 /* , 15 */] {
        let sig_tx = sig_tx.clone();

        signals.push(Event::new(sig_id, move |sig_num| {
            // TODO: can I translate from i32 to human-readable name..?
            info!("Received a signal, num {}", sig_num);
            sig_tx.send(CallbackComms::Shutdown).unwrap();
        }));
        trace!("configuring signal handler for {}", sig_id);
    }

    mainloop.init_signals()?;
    Ok(signals)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logs();

    let (tx, rx) = mpsc::channel();
    let mut mainloop = Mainloop::new().ok_or("mainloop new failed")?;
    let _sig_events = setup_unix_signals(&mut mainloop, tx.clone())?;

    let proplist = Proplist::new().ok_or("proplist failed")?;
    let mut context = Context::new_with_proplist(&mainloop, "source-listener", &proplist)
        .ok_or("context::new_with_proplist failed")?;

    info!("Connecting to daemon");
    connect_to_server(&mut context, &mut mainloop, tx.clone(), &rx)?;

    debug!("We should be connected at this point..!");

    let state = ListenerState::new(&mut mainloop, &mut context, tx.clone(), &rx)?;
    let subscribe_result = subscribe_source_mute(mainloop, context, state, tx.clone(), rx);
    match subscribe_result {
        Ok(_) => Ok(()),
        Err(err) => match err {
            Errors::Shutdown => Ok(()),
            _ => Err(Box::new(err)),
        },
    }
}

fn get_sources(
    context: &Context,
    mainloop: &mut Mainloop,
    tx: CBTX,
    rx: &CBRX,
) -> Result<Sources, Errors> {
    // Lock mainloop to block pulseaudio from calling things during setup
    mainloop.lock();

    let introspector = context.introspect();

    introspector.get_source_info_list(move |src| match src {
        ListResult::Error => {
            error!("Failed to retrieve ListResult");
            tx.send(CallbackComms::SrcElem(SrcListState::Err)).unwrap();
        }
        ListResult::End => {
            tx.send(CallbackComms::SrcElem(SrcListState::Done)).unwrap();
        }
        ListResult::Item(item) => {
            let source_name = match &item.name {
                None => "unknown".to_string(),
                Some(name) => name.to_string(),
            };

            tx.send(CallbackComms::SrcElem(SrcListState::Item(
                item.index,
                SourceDatum::new(source_name, item.mute),
            )))
            .unwrap();
        }
    });

    let mut sources = HashMap::new();

    // Unlock mainloop to let pulseaudio call the above callback.
    mainloop.unlock();
    loop {
        let event = rx.recv()?;

        match event {
            CallbackComms::Shutdown => {
                return Err(Errors::Shutdown);
            }
            CallbackComms::SrcElem(elem) => match elem {
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
            },
            _ => panic!("impossible state {:?}", event),
        }
    }
}

fn find_default_source_name(
    context: &mut Context,
    mainloop: &mut Mainloop,
    tx: CBTX,
    rx: &CBRX,
) -> Result<String, Errors> {
    // Block pulseaudio from inboking callbacks
    mainloop.lock();

    let introspector = context.introspect();

    {
        introspector.get_server_info(move |server_info| {
            trace!("Server info: {:?}", server_info);
            match &server_info.default_source_name {
                None => {
                    info!("no default source");
                    tx.send(CallbackComms::ServerInfo(DefaultSourceState::NoDefault))
                        .unwrap()
                }
                Some(value) => {
                    info!("Default source: '{:?}'", value);
                    tx.send(CallbackComms::ServerInfo(DefaultSourceState::Default(
                        value.to_string(),
                    )))
                    .unwrap();
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
            CallbackComms::Shutdown => {
                return Err(Errors::Shutdown);
            }
            CallbackComms::ServerInfo(default_source) => {
                trace!("Grabbed default source");
                match default_source {
                    DefaultSourceState::NoDefault => {
                        return Ok("No default source".to_owned());
                    }
                    DefaultSourceState::Default(name) => {
                        trace!("Returning from get_sources");
                        return Ok(name.to_owned());
                    }
                };
            }
            _ => panic!("impossible state {:?}", event),
        }
    }
}

fn get_default_source_index(
    mainloop: &mut Mainloop,
    context: &mut Context,
    sources: &Sources,
    tx: CBTX,
    rx: &CBRX,
) -> Result<u32, Errors> {
    let default_source_name = find_default_source_name(context, mainloop, tx, rx)?;

    for (index, source) in sources {
        if source.name == default_source_name {
            debug!("Default source is: '{}', index: {}", source.name, index);
            return Ok(*index);
        }
    }

    error!("failed to set default source");
    Err(Errors::ContextError("failed to set default source".into()))
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
                "{} [{}:{}] ({}): {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S%.6f%z"),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.level(),
                record.args(),
            )
        })
        .init();
}

fn subscribe_source_mute(
    mut mainloop: Mainloop,
    mut context: Context,
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
                                // if state.default_source == id {
                                // trace!("Default source changed config");
                                // let old_mute_state = state.default_source().unwrap().mute;

                                // tell callback that mainloop should update sources (can't do that here since
                                // we're already inside a callback).
                                // trace!("Source {} Changed", idx);
                                tx.send(CallbackComms::ChangeType(Facility::Source))
                                    .unwrap();
                            }
                            Operation::New => {
                                debug!("New source added with index {}", idx);
                            }
                            Operation::Removed => {
                                debug!("Source with index {} removed", idx);
                            }
                        }
                    }
                    Facility::Server => {
                        info!("Server change event");
                        let _ = tx.send(CallbackComms::ChangeType(Facility::Server));
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

    // TODO: We should also bind to shutdown signal for clean teardown here...
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
        trace!("current source mute state: {:?}", &old_default_mute);

        let event = rx.recv()?;
        match event {
            CallbackComms::Shutdown => {
                return Err(Errors::Shutdown);
            }
            CallbackComms::ChangeType(change) => {
                match change {
                    Facility::Server => {
                        let _ = handle_server_change(
                            &mut state,
                            &mut mainloop,
                            &mut context,
                            tx.clone(),
                            &rx,
                        );
                        // Always check source changes, to ensure the new default's mute state is compared
                        // against prior mute state.
                        state.sources =
                            get_sources(&context, &mut mainloop, tx.clone(), &rx).unwrap();
                    }
                    Facility::Source => {
                        state.sources =
                            get_sources(&context, &mut mainloop, tx.clone(), &rx).unwrap();
                    }
                    _ => panic!("impossible state {:?}", change),
                }
            }
            _ => panic!("impossible state {:?}", event),
        }

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
}

fn handle_server_change(
    state: &mut ListenerState,
    mainloop: &mut Mainloop,
    context: &mut Context,
    tx: CBTX,
    rx: &CBRX,
) -> Result<(), Errors> {
    // Check if default source changed and update state
    debug!("Updating default source after server config change");
    // TODO: Do we need to check if the source map needs updating...?
    // Ideally - we collect sources once on start, then use the source add/remove subscriptions to
    // keep updated...
    state.default_source = get_default_source_index(mainloop, context, &state.sources, tx, rx)?;

    debug!(
        "Default source is now: {}",
        state.default_source().unwrap().name
    );

    Ok(())
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
        trace!("Waiting for context state-change callback");
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
        trace!("received");

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
