use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::{
    cell::{RefCell, RefMut},
    error::Error,
    ops::Deref,
};

use chrono::Local;
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
    mainloop::threaded::Mainloop,
    proplist::Proplist,
};

type RContext = Rc<RefCell<Context>>;
type RMainloop = Rc<RefCell<Mainloop>>;
type RState = Rc<RefCell<ListenerState>>;

type Sources = HashMap<u32, SourceDatum>;

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
struct ListenerState {
    // Use Pulseaudio's source index as key to source data (which is just name and mute-status)
    sources: Sources,
    default_source: u32,
}

impl ListenerState {
    fn new(mainloop: &RMainloop, context: &RContext) -> Result<Self, Box<dyn Error>> {
        let sources = get_sources(context, mainloop)?;
        let default_source = get_default_source(mainloop, context, &sources)?;
        Ok(Self {
            sources,
            default_source,
        })
    }

    fn default_source<'a>(&'a self) -> Option<&'a SourceDatum> {
        self.sources.get(&self.default_source)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logs();

    let mainloop = Rc::new(RefCell::new(Mainloop::new().ok_or("mainloop new failed")?));

    let proplist = Proplist::new().ok_or("proplist failed")?;
    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(mainloop.borrow_mut().deref(), "source-listener", &proplist)
            .ok_or("context::new_with_proplist failed")?,
    ));

    info!("Connecting to daemon");
    connect_to_server(&context, &mainloop)?;

    debug!("We should be connected at this point..!");

    let state = Rc::new(RefCell::new(ListenerState::new(&mainloop, &context)?));
    subscribe_source_mute(mainloop, context, state)
}

enum SrcListState {
    InProg,
    Done,
    Err(String),
}

fn get_sources(context: &RContext, mainloop: &RMainloop) -> Result<Sources, Box<dyn Error>> {
    trace!("get sources aquiring mainloop lock");
    mainloop.borrow_mut().lock();
    trace!("mainloop locked");

    let introspector = context.borrow_mut().introspect();

    let done = Rc::new(RefCell::new(SrcListState::InProg));
    let sources = Rc::new(RefCell::new(HashMap::new()));

    {
        let sources = sources.clone();
        let done = Rc::clone(&done);

        let mainloop = Rc::clone(&mainloop);

        introspector.get_source_info_list(move |src| match src {
            ListResult::Error => {
                let msg = "Failed to retrieve ListResult".into();
                error!("{}", msg);
                *done.borrow_mut() = SrcListState::Err(msg);
                unsafe { (*mainloop.as_ptr()).signal(false) };
            }
            ListResult::End => {
                *done.borrow_mut() = SrcListState::Done;
                unsafe { (*mainloop.as_ptr()).signal(false) };
            }
            ListResult::Item(item) => {
                let source_name = match &item.name {
                    None => "unknown".to_string(),
                    Some(name) => name.to_string(),
                };

                sources
                    .borrow_mut()
                    .insert(item.index, SourceDatum::new(source_name, item.mute));
            }
        });
    }

    loop {
        debug!("Waiting on mainloop signal");
        mainloop.borrow_mut().wait();
        match done.borrow_mut().deref() {
            SrcListState::InProg => continue,
            SrcListState::Done => {
                trace!("get sources unlocking mainloop lock");
                mainloop.borrow_mut().unlock();
                return Ok(sources.borrow().deref().clone());
            }
            SrcListState::Err(err) => {
                error!("Caught error waiting: {}", err);
                trace!("get sources unlocking mainloop lock");
                mainloop.borrow_mut().unlock();
                return Err(err.to_owned().into());
            }
        }
    }
}

#[derive(Debug, Clone)]
enum DefaultSourceState {
    NoDefault,
    Default(String),
    Unset,
}

fn find_default_source_name(
    context: &RContext,
    mainloop: &RMainloop,
) -> Result<String, Box<dyn Error>> {
    mainloop.borrow_mut().lock();
    let introspector = context.borrow_mut().introspect();

    let default_source = Arc::new(Mutex::new(DefaultSourceState::Unset));

    let default_source_inner = default_source.clone();
    let mainloop_inner = Rc::clone(&mainloop);
    introspector.get_server_info(move |server_info| {
        trace!("Server info: {:?}", server_info);
        let mut default_source = default_source_inner.lock().unwrap();
        match &server_info.default_source_name {
            None => *default_source = DefaultSourceState::NoDefault,
            Some(value) => {
                *default_source = DefaultSourceState::Default(value.to_string());
            }
        };
        info!("Default source: '{:?}'", *default_source);
        unsafe { (*mainloop_inner.as_ptr()).signal(false) };
    });

    loop {
        trace!("grabbing default source value");
        let default_source = {
            let source_inner = default_source.lock().unwrap();
            source_inner.deref().clone()
        };
        trace!("source value grabbed...");
        match default_source {
            DefaultSourceState::Unset => {
                mainloop.borrow_mut().wait();
            }
            DefaultSourceState::NoDefault => {
                mainloop.borrow_mut().unlock();
                return Ok("No default source".to_owned());
            }
            DefaultSourceState::Default(name) => {
                mainloop.borrow_mut().unlock();
                return Ok(name.to_owned());
            }
        };
    }
}

fn get_default_source(
    mainloop: &RMainloop,
    context: &RContext,
    sources: &Sources,
) -> Result<u32, Box<dyn Error>> {
    let default_source_name = find_default_source_name(context, mainloop)?;

    for (index, source) in sources {
        if source.name == default_source_name {
            debug!("Default source is: '{}', index: {}", source.name, index);
            return Ok(*index);
        }
    }

    error!("failed to set default source");
    Err("failed to set default source".into())
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
                Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%z"),
                record.file().unwrap_or("unknown"),
                record.line().unwrap_or(0),
                record.level(),
                record.args(),
            )
        })
        .init();
}

fn subscribe_source_mute(
    mainloop: RMainloop,
    context: RContext,
    state: Rc<RefCell<ListenerState>>,
) -> Result<(), Box<dyn Error>> {
    // Sources toggle their mute state, default source changes Server state
    let source_mask = InterestMaskSet::SOURCE | InterestMaskSet::SERVER;

    trace!("Configuring context subscriber");
    mainloop.borrow_mut().lock();
    // tell pulseaudio to notify us about Source & Server changes
    {
        // set callback that reacts to subscription changes
        let mainloop = mainloop.clone();
        let context_inner = context.clone();
        let state = state.clone();
        context.borrow_mut().set_subscribe_callback(Some(Box::new(
            move |facility: Option<Facility>, operation: Option<Operation>, idx| {
                let facility = facility.unwrap();
                let operation = operation.unwrap();
                debug!(
                    "Subcribe callback: {:?}, {:?}, {:?}",
                    facility, operation, idx
                );

                match facility {
                    Facility::Source => {
                        debug!("Source event");
                        if let Ok(Some(())) =
                            handle_source_change(Rc::clone(&state).borrow_mut(), operation, idx)
                        {
                            trace!("mainloop should update sources");
                            unsafe { (*mainloop.as_ptr()).signal(false) };
                        } else {
                            trace!("source event unrelated to default source, no need to update.");
                        }
                    }
                    Facility::Server => {
                        info!("Server change event");
                        let _ = handle_server_change(
                            state.clone(),
                            mainloop.clone(),
                            context_inner.clone(),
                        );
                    }
                    _ => debug!("Unrelated event: {:?}", facility),
                }
            },
        )));
    }

    context.borrow_mut().subscribe(source_mask, |sub_success| {
        debug!(
            "Subscribing to source changes {}",
            match sub_success {
                true => "succeeded",
                false => "failed",
            }
        );
    });

    // TODO: We should also bind to shutdown signal for clean teardown here...
    loop {
        mainloop.borrow_mut().wait();
        trace!("Caught signal in subscribe loop");

        // When we are signalled here, it means, we should update sources, and then print if the
        // mute state of the default source, changed.

        let old_default_mute = state.borrow().default_source().unwrap().mute;
        trace!("old default mute: {}, updating sources", old_default_mute);
        trace!("releasing mainloop lock.");
        mainloop.borrow_mut().unlock();
        state.borrow_mut().sources =
            get_sources(Rc::clone(&context), Rc::clone(&mainloop)).unwrap();

        trace!("re-aquiring mainloop lock.");
        mainloop.borrow_mut().lock();

        if old_default_mute != state.borrow().default_source().unwrap().mute {
            println!(
                "{}",
                match state.borrow().default_source().unwrap().mute {
                    true => "MUTED",
                    false => "UNMUTED",
                }
            );
        }
    }
}

fn handle_server_change(
    state: &RState,
    mainloop: &RMainloop,
    context: &RContext,
) -> Result<(), Box<dyn Error>> {
    // Check if default source changed and update state
    debug!("Updating default source after server config change");
    // TODO: Do we need to check if the source map needs updating...?
    // Ideally - we collect sources once on start, then use the source add/remove subscriptions to
    // keep updated...
    state.borrow_mut().default_source = get_default_source(
        mainloop.clone(),
        context.clone(),
        &state.borrow_mut().sources,
    )?;

    debug!(
        "Default source is now: {}",
        state.borrow().default_source().unwrap().name
    );

    Ok(())
}

fn handle_source_change(
    state: RefMut<ListenerState>,
    operation: Operation,
    id: u32,
) -> Result<Option<()>, Box<dyn Error>> {
    // Check the index of the source that changed here against the default
    // source, if the default source changed, check if mute was toggled and
    // report...
    trace!("Handling SourceChange event for index {}", id);
    match operation {
        Operation::Changed => {
            if state.default_source == id {
                trace!(
                    "Default source changed config, old_state: {}",
                    state.default_source().unwrap().mute
                );
                // let old_mute_state = state.default_source().unwrap().mute;

                // tell callback that mainloop should update sources (can't do that here since
                // we're already inside a callback).
                return Ok(Some(()));
            } else {
                debug!("Something changed on a non-default source, ignoring");
            }
        }
        Operation::New => {
            debug!("New source added with index {}", id);
        }
        Operation::Removed => {
            debug!("Source with index {} removed", id);
        }
    }
    Ok(None)
}

fn connect_to_server(context: &RContext, mainloop: &RMainloop) -> Result<(), Box<dyn Error>> {
    trace!("Calling context.connect");

    {
        // Context state boxed-callback setup
        let mainloop_inner = Rc::clone(&mainloop);
        trace!("Registering context state callback");
        context
            .borrow_mut()
            .set_state_callback(Some(Box::new(move || {
                trace!("context state changed");
                // Should be safe IFF we handle mainloop locks properly...
                unsafe { (*mainloop_inner.as_ptr()).signal(false) };
            })));
    }

    context
        .borrow_mut()
        .connect(None, FlagSet::NOAUTOSPAWN, None)?;

    mainloop.borrow_mut().lock();
    mainloop.borrow_mut().start()?;

    loop {
        trace!("Waiting for mainloop signal to indicate state-change");

        match context.borrow().get_state() {
            State::Unconnected | State::Connecting | State::Authorizing | State::SettingName => {
                debug!("Context connecting to server");

                mainloop.borrow_mut().wait();
            }
            State::Ready => {
                debug!("Context connected and ready");
                break;
            }
            State::Failed => {
                error!("Context failed to connect, exiting.");
                mainloop.borrow_mut().unlock();
                return Err("Context connect failed".into());
            }
            State::Terminated => {
                info!("Context was terminated, quitting");
                mainloop.borrow_mut().unlock();
                return Err("Context terminated".into());
            }
        }
    }
    // Once connected, we don't care anymore...
    context.borrow_mut().set_state_callback(None);

    mainloop.borrow_mut().unlock();

    Ok(())
}
