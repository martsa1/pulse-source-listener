use std::{cell::RefCell, error::Error, process::exit, rc::Rc};

use clap::Parser;
use env_logger::Env;
use log::{debug, error, info, trace};
use pulse::{
    callbacks::ListResult,
    context::{Context, FlagSet, State},
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let log_env = if args.verbose {
        Env::default().default_filter_or("debug")
    } else {
        Env::default().default_filter_or("info")
    };
    env_logger::Builder::from_env(log_env).init();

    let mut mainloop = Mainloop::new().ok_or("mailoop new failed")?;

    let proplist = Proplist::new().ok_or("proplist failed")?;
    let mut context = Context::new_with_proplist(&mainloop, "source-listener", &proplist)
        .ok_or("context::new_with_proplist failed")?;

    info!("Connecting to daemon");
    connect_to_server(&mut context, &mut mainloop)?;

    debug!("We should be connected at this point..!");

    let done_flag = Rc::new(RefCell::new(0 as u8));
    let introspector = context.introspect();

    let cb_done = done_flag.clone();
    introspector.get_source_info_list(move |source_lst_res| match source_lst_res {
        ListResult::Item(src) => {
            trace!("Found source: {:?}", src);
        }
        ListResult::Error => {
            error!("Something went wrong analysing source info");
        }
        ListResult::End => {
            info!("End of list, signalling quit to mainloop");
            *cb_done.borrow_mut() += 1;
        }
    });

    let cb_done = done_flag.clone();
    introspector.get_server_info(move |server_info| {
        trace!("Server info: {:?}", server_info);
        info!("Default source: {:?}", server_info.default_source_name);
        *cb_done.borrow_mut() += 1;
    });

    loop {
        // Use Mainloop iterate to process data from pulseaudio server, this iterate is what
        // executes our various callbacks etc. (true here blocks mainloop to wait for events)
        match mainloop.iterate(true) {
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

                let cbs_done = *done_flag.borrow();
                trace!("{} of 2 introspection callbacks completed", &cbs_done);
                if cbs_done >= 2 {
                    debug!("Done flagset, quitting");
                    break;
                }
            }
        };
    }

    Ok(())
}

fn connect_to_server(context: &mut Context, mainloop: &mut Mainloop) -> Result<(), Box<dyn Error>> {
    context.connect(None, FlagSet::NOAUTOSPAWN, None)?;
    loop {
        // Use Mainloop iterate to process data from pulseaudio server, this iterate is what
        // executes our various callbacks etc. (true here blocks mainloop to wait for events)
        match mainloop.iterate(true) {
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
                    debug!("Iterate succeeded, dispatched {} events", num_dispatched);
                }

                match context.get_state() {
                    State::Unconnected
                    | State::Connecting
                    | State::Authorizing
                    | State::SettingName => {
                        debug!("Context connecting to server");
                    }

                    State::Ready => {
                        debug!("Context connected and ready");
                        return Ok(());
                    }

                    State::Failed => {
                        error!("Context failed to connect, exiting.");
                        return Err("Failed to connect to server".into());
                    }
                    State::Terminated => {
                        info!("Context was terminated cleanly, quitting");
                        return Err("Terminated".into());
                    }
                }
            }
        };
    }
}
