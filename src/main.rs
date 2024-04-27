use std::{
    cell::{RefCell, RefMut},
    error::Error,
    ops::Deref,
    process::exit,
    rc::Rc,
};

use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, trace};
use pulse::{
    callbacks::ListResult,
    context::{
        subscribe::{Facility, InterestMaskSet, Operation},
        Context, FlagSet, State,
    },
    mainloop::standard::{IterateResult, Mainloop},
    proplist::Proplist,
};

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
    index: i32,
    mute: bool,
}
impl SourceDatum {
    fn new(name: String, index: i32, mute: bool) -> Self {
        SourceDatum {
            name: name.to_string(),
            index,
            mute,
        }
    }
}

#[derive(Debug)]
enum Event {
    ServerChange,
    SourceChange(u32),
}

#[derive(Debug)]
struct ListenerState {
    sources: Vec<SourceDatum>,
    default_source: SourceDatum,
}

type RContext = Rc<RefCell<Context>>;
type RMainloop = Rc<RefCell<Mainloop>>;
type RState = Rc<RefCell<ListenerState>>;

impl ListenerState {
    fn new(mainloop: RMainloop, context: RContext) -> Result<Self, Box<dyn Error>> {
        let sources = source_list(context.clone(), mainloop.clone())?;
        let default_source = get_default_source(mainloop, context, &sources)?;
        Ok(Self {
            sources,
            default_source,
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    setup_logs();

    let mainloop = Rc::new(RefCell::new(Mainloop::new().ok_or("mailoop new failed")?));

    let proplist = Proplist::new().ok_or("proplist failed")?;
    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(mainloop.borrow().deref(), "source-listener", &proplist)
            .ok_or("context::new_with_proplist failed")?,
    ));

    info!("Connecting to daemon");
    connect_to_server(context.clone(), mainloop.clone())?;

    debug!("We should be connected at this point..!");

    let state = Rc::new(RefCell::new(ListenerState::new(
        mainloop.clone(),
        context.clone(),
    )?));
    subscribe_source_mute(mainloop, context, state)
}

fn source_list(context: RContext, mainloop: RMainloop) -> Result<Vec<SourceDatum>, Box<dyn Error>> {
    let introspector = context.borrow().introspect();

    let sources = Rc::new(RefCell::new(vec![]));
    let source_iter_done = Rc::new(RefCell::new(None));

    let sources_inner = sources.clone();
    let source_iter_done_inner = source_iter_done.clone();
    introspector.get_source_info_list(move |src| match src {
        ListResult::Error => {
            *source_iter_done_inner.borrow_mut() = Some(Err("Error collecting source info list"));
        }
        ListResult::End => {
            *source_iter_done_inner.borrow_mut() = Some(Ok(()));
        }
        ListResult::Item(item) => {
            let source_name = match &item.name {
                None => "unknown".to_string(),
                Some(name) => name.to_string(),
            };

            sources_inner.borrow_mut().push(SourceDatum::new(
                source_name,
                item.index.try_into().unwrap(),
                item.mute,
            ));
        }
    });

    let source_iter_done_inner = source_iter_done.clone();
    iter_loop_to_done(mainloop, context, move |_| {
        // Check if source_iter_done is an Err or Ok
        match source_iter_done_inner.borrow().deref() {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    })?;

    let source_iter_result = source_iter_done.borrow().unwrap();
    match source_iter_result {
        Ok(_) => {
            return Ok(sources.take());
        }
        Err(err) => {
            return Err(err.into());
        }
    }
}

fn find_default_source_name(
    context: RContext,
    mainloop: RMainloop,
) -> Result<String, Box<dyn Error>> {
    let introspector = context.borrow().introspect();

    let default_source = Rc::new(RefCell::new(None));

    let default_source_inner = default_source.clone();
    introspector.get_server_info(move |server_info| {
        trace!("Server info: {:?}", server_info);
        let mut default_source = default_source_inner.borrow_mut();
        match &server_info.default_source_name {
            None => {}
            Some(value) => {
                *default_source = Some(value.to_string());
            }
        };
        info!("Default source: '{:?}'", *default_source);
    });

    iter_loop_to_done(mainloop, context, |_| {
        let default_source = default_source.borrow();
        match *default_source {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    })?;
    default_source.take().ok_or("No default source".into())
}

fn get_default_source(
    mainloop: RMainloop,
    context: RContext,
    sources: &Vec<SourceDatum>,
) -> Result<SourceDatum, Box<dyn Error>> {
    let default_source_name = find_default_source_name(context, mainloop)?;

    for source in sources {
        if source.name == default_source_name {
            debug!(
                "Default source is: '{}', index: {}",
                source.name, source.index
            );
            return Ok(source.clone());
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
    env_logger::Builder::from_env(log_env).init();
}

fn subscribe_source_mute(
    mainloop: RMainloop,
    context: RContext,
    state: Rc<RefCell<ListenerState>>,
) -> Result<(), Box<dyn Error>> {
    // Sources toggle their mute state, default source changes Server state
    let source_mask = InterestMaskSet::SOURCE | InterestMaskSet::SERVER;

    // tell pulseaudio to notify us about Source & Server changes
    context.borrow_mut().subscribe(source_mask, |sub_success| {
        debug!(
            "Subscribing to source changes {}",
            match sub_success {
                true => "succeeded",
                false => "failed",
            }
        );
    });

    // set callback that reacts to subscription changes
    let mainloop_inner = mainloop.clone();
    let context_inner = context.clone();
    let state_inner = state.clone();
    let mut mut_context = context.borrow_mut();
    mut_context.set_subscribe_callback(Some(Box::new(
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
                    let _ = handle_source_change(Rc::clone(&state).borrow_mut(), operation, idx);
                }
                Facility::Server => {
                    info!("Server change event");
                    let _ = handle_server_change(
                        state_inner.clone(),
                        mainloop_inner.clone(),
                        context_inner.clone(),
                    );
                }
                _ => debug!("Unrelated event: {:?}", facility),
            }
        },
    )));

    // Run loop listening to changes
    // The closure here essentially is run in the loop, with some boilerplate to react to loop
    // shutdown or error etc.
    //
    // TODO: let this gracefully exit on signal
    match mainloop.clone().borrow_mut().run() {
        Ok(_) => Ok(()),
        Err((pa_err, retval)) => {
            error!(
                "Encountered error whilst subscribed to pulseaudio: '{}', return value: {:?}",
                pa_err, retval
            );
            Err(Box::new(pa_err))
        }
    }
}

fn handle_server_change(
    state: RState,
    mainloop: RMainloop,
    context: RContext,
) -> Result<(), Box<dyn Error>> {
    // Check if default source changed and update state
    debug!("Updating default source after server config change");
    state.borrow_mut().default_source =
        get_default_source(mainloop.clone(), context.clone(), &state.borrow().sources)?;

    debug!(
        "Default source is now: {}",
        state.borrow().default_source.name
    );

    Ok(())
}

fn handle_source_change(
    state: RefMut<ListenerState>,
    operation: Operation,
    id: u32,
) -> Result<(), Box<dyn Error>> {
    // Check the index of the source that changed here against the default
    // source, if the default source changed, check if mute was toggled and
    // report...
    trace!("Handling SourceChange event for index {}", id);
    match operation {
        Operation::Changed => {
            if state.default_source.index == id as i32 {
                trace!("Default source changed config");
                let old_mute_state = state.default_source.mute;

                // This gets the full source list and server data on every call which feels pretty
                // wasteful.
                //
                // Should be able to optimise so that that data is only actually calculated when we detect
                // new/removed sources (which could change indexes), or on new default source (already
                // implemented).
                // state.default_source = get_default_source(mainloop.clone(), context.clone(), state.sources)?;

                if old_mute_state != state.default_source.mute {
                    println!(
                        "{}",
                        match state.default_source.mute {
                            true => "MUTED",
                            false => "UNMUTED",
                        }
                    );
                }
            } else {
                debug!("Something changed on a non-default source");
            }
        }
        Operation::New => {
            debug!("New source added with index {}", id);
        }
        Operation::Removed => {
            debug!("Source with index {} removed", id);
        }
    }
    Ok(())
}

fn iter_loop_to_done(
    mainloop: RMainloop,
    context: RContext,
    mut done_chk: impl FnMut(RContext) -> Result<bool, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    loop {
        // Use Mainloop iterate to process data from pulseaudio server, this iterate is what
        // executes our various callbacks etc. (true here blocks mainloop to wait for events)
        match mainloop.borrow_mut().iterate(true) {
            IterateResult::Err(err) => {
                error!("Caught error running mainloop: {}", err);
                exit(1);
            }
            IterateResult::Quit(retval) => {
                info!("Quit called, retval: {:?}", retval);
                exit(retval.0);
            }
            IterateResult::Success(num_dispatched) => {
                if num_dispatched > 0 {
                    trace!("Iterate succeeded, dispatched {} events", num_dispatched);
                }

                if done_chk(context.clone())? {
                    return Ok(());
                }
            }
        };
    }
}

fn connect_to_server(context: RContext, mainloop: RMainloop) -> Result<(), Box<dyn Error>> {
    context
        .borrow_mut()
        .connect(None, FlagSet::NOAUTOSPAWN, None)?;
    iter_loop_to_done(mainloop, context, |context| {
        match context.borrow().get_state() {
            State::Unconnected | State::Connecting | State::Authorizing | State::SettingName => {
                debug!("Context connecting to server");
                return Ok(false);
            }

            State::Ready => {
                debug!("Context connected and ready");
                return Ok(true);
            }

            State::Failed => {
                error!("Context failed to connect, exiting.");
                return Ok(false);
            }
            State::Terminated => {
                info!("Context was terminated cleanly, quitting");
                return Ok(false);
            }
        }
    })?;
    Ok(())
}
