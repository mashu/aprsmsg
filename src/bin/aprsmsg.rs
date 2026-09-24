//! aprsmsg — interactive APRS messaging client (native CLI).

use std::env;
use std::io::{self, BufRead};
use std::process;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use aprsmsg::client::{Action, Client, ClientConfig, message_group_filter, COMMANDS};
use aprsmsg::heard::passcode;
use aprsmsg::link::{AprsIsLogin, Link, LinkEvent};
use aprsmsg::ui::Ui;
use aprsmsg::ax25::Address;

const LINGER: Duration = Duration::from_secs(30);
const IDLE_WAIT: Duration = Duration::from_secs(3600);
const DEFAULT_KISS: &str = "127.0.0.1:8001";
const DEFAULT_APRS_IS: &str = "euro.aprs2.net:14580";

const USAGE: &str = "\
usage: aprsmsg --call MYCALL [radio or internet options] [--monitor] [--no-color]

  --call MYCALL      your callsign or callsign-SSID, e.g. SA0KAM or SA0KAM-1
                     (required; bare callsign receives messages to every SSID)

radio (default), through Direwolf:
  --kiss HOST:PORT   Direwolf KISS TCP port                   (default 127.0.0.1:8001)
  --chan N           radio channel, 0 = first                 (default 0)
  --path P           digipeater path, or \"none\"               (default WIDE1-1,WIDE2-1)

internet, no radio needed:
  --aprs-is          connect to an APRS-IS server instead of Direwolf
  --server HOST:PORT APRS-IS server                           (default euro.aprs2.net:14580)
  --passcode N       APRS-IS passcode                         (default: computed from --call)
  --filter F         extra server filter for the monitor, e.g. r/59.33/18.07/50
                     (messages to MYCALL are always received)

common:
  --tocall T         AX.25 destination (software identifier)  (default APZRST)
  --monitor          show other stations' packets from the start
  --no-color         plain output (also when NO_COLOR is set or output is piped)";

enum Transport {
    Radio { kiss: String, chan: u8 },
    Internet {
        server: String,
        passcode: u16,
        filter: String,
    },
}

struct Config {
    call: Address,
    transport: Transport,
    path: Vec<Address>,
    tocall: Address,
    monitor: bool,
    no_color: bool,
}

fn parse_args() -> Result<Config, String> {
    let mut call = None;
    let mut kiss = String::from(DEFAULT_KISS);
    let mut chan: u8 = 0;
    let mut path = String::from("WIDE1-1,WIDE2-1");
    let mut aprs_is = false;
    let mut server = String::from(DEFAULT_APRS_IS);
    let mut passcode_opt: Option<u16> = None;
    let mut filter = String::new();
    let mut tocall = String::from("APZRST");
    let mut monitor = false;
    let mut no_color = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--call" => call = Some(value_of(&mut args, "--call")?),
            "--kiss" => kiss = value_of(&mut args, "--kiss")?,
            "--chan" => {
                chan = value_of(&mut args, "--chan")?
                    .parse()
                    .ok()
                    .filter(|&c| c <= 15)
                    .ok_or("--chan must be 0-15")?
            }
            "--path" => path = value_of(&mut args, "--path")?,
            "--aprs-is" => aprs_is = true,
            "--server" => server = value_of(&mut args, "--server")?,
            "--passcode" => {
                passcode_opt = Some(
                    value_of(&mut args, "--passcode")?
                        .parse()
                        .map_err(|_| "--passcode must be a number")?,
                )
            }
            "--filter" => filter = value_of(&mut args, "--filter")?,
            "--tocall" => tocall = value_of(&mut args, "--tocall")?,
            "--monitor" => monitor = true,
            "--no-color" => no_color = true,
            "-h" | "--help" => {
                println!("{USAGE}\n\n{COMMANDS}");
                process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let call = Address::parse(&call.ok_or("--call is required")?)?;
    let path = if path.trim().is_empty() || path.eq_ignore_ascii_case("none") {
        Vec::new()
    } else {
        path.split(',')
            .map(Address::parse)
            .collect::<Result<Vec<_>, _>>()?
    };
    if path.len() > 8 {
        return Err("--path allows at most 8 digipeaters".into());
    }
    let tocall = Address::parse(&tocall)?;
    let transport = if aprs_is {
        Transport::Internet {
            server,
            passcode: passcode_opt.unwrap_or_else(|| passcode(&call.call)),
            filter: message_group_filter(&call, &filter),
        }
    } else {
        Transport::Radio { kiss, chan }
    };

    Ok(Config {
        call,
        transport,
        path,
        tocall,
        monitor,
        no_color,
    })
}

fn value_of(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next().ok_or(format!("{name} needs a value"))
}

fn open_link(cfg: &Config) -> io::Result<(Link, String)> {
    match &cfg.transport {
        Transport::Radio { kiss, chan } => {
            let link = Link::kiss(kiss, *chan)?;
            Ok((link, format!("radio via Direwolf {kiss}, channel {chan}")))
        }
        Transport::Internet {
            server,
            passcode,
            filter,
        } => {
            let call = cfg.call.to_string();
            let login = AprsIsLogin {
                server,
                call: &call,
                passcode: *passcode,
                filter,
            };
            let link = Link::aprs_is(&login)?;
            Ok((link, format!("APRS-IS {server}, filter {filter}")))
        }
    }
}

enum Event {
    Link(LinkEvent),
    Line(String),
    InputClosed,
}

fn spawn_stdin_reader(events: Sender<Event>) {
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if events.send(Event::Line(line)).is_err() {
                return;
            }
        }
        let _ = events.send(Event::InputClosed);
    });
}

fn apply_actions(
    actions: Vec<Action>,
    client: &Client,
    link: &mut Link,
    ui: &Ui,
) -> io::Result<bool> {
    for action in actions {
        match action {
            Action::Send(info) => {
                link.send(client.call(), client.tocall(), client.path(), &info)?;
            }
            Action::Ui(msg) => ui.apply(&msg),
            Action::Quit => return Ok(false),
        }
    }
    Ok(true)
}

fn run(cfg: Config) -> io::Result<()> {
    let no_color = cfg.no_color;
    let radio = matches!(cfg.transport, Transport::Radio { .. });
    let (mut link, description) = open_link(&cfg)?;
    let client_cfg = ClientConfig {
        call: cfg.call.clone(),
        path: cfg.path,
        tocall: cfg.tocall,
        monitor: cfg.monitor,
        radio,
    };

    let (events_tx, events) = mpsc::channel();
    let link_tx = events_tx.clone();
    link.spawn_reader(move |event| link_tx.send(Event::Link(event)).is_ok())?;
    spawn_stdin_reader(events_tx);

    let ui = Ui::new(no_color);
    let mut client = Client::new(client_cfg);
    ui.apply(&aprsmsg::UiMsg::Info(format!(
        "{} on {description} — type help",
        client.call()
    )));

    let mut input_open = true;
    let mut linger_until: Option<Instant> = None;

    loop {
        let mut timeout = IDLE_WAIT;
        if let Some(until_retry) = client.time_until_deadline() {
            timeout = timeout.min(until_retry);
        }
        if let Some(link_at) = link.next_deadline() {
            timeout = timeout.min(link_at.saturating_duration_since(Instant::now()));
        }
        if let Some(linger_at) = linger_until {
            timeout = timeout.min(linger_at.saturating_duration_since(Instant::now()));
        }
        match events.recv_timeout(timeout) {
            Ok(Event::Link(LinkEvent::Heard(heard))) => {
                let actions = client.on_heard(heard);
                apply_actions(actions, &client, &mut link, &ui)?;
            }
            Ok(Event::Link(LinkEvent::Notice(notice))) => {
                let actions = client.on_notice(&notice);
                apply_actions(actions, &client, &mut link, &ui)?;
            }
            Ok(Event::Link(LinkEvent::Closed(reason))) => {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, reason));
            }
            Ok(Event::Line(line)) => {
                let actions = client.on_command(&line);
                if !apply_actions(actions, &client, &mut link, &ui)? {
                    return Ok(());
                }
            }
            Ok(Event::InputClosed) => {
                if client.pending_count() == 0 {
                    return Ok(());
                }
                input_open = false;
                ui.apply(&aprsmsg::UiMsg::Info(format!(
                    "input closed; waiting for {} pending message(s)",
                    client.pending_count()
                )));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        link.maintain()?;
        let actions = client.poll();
        apply_actions(actions, &client, &mut link, &ui)?;

        if !input_open && client.pending_count() == 0 {
            let until = *linger_until.get_or_insert_with(|| Instant::now() + LINGER);
            if Instant::now() >= until {
                return Ok(());
            }
        }
    }
}

fn main() {
    let cfg = match parse_args() {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("aprsmsg: {msg}\n\n{USAGE}");
            process::exit(2);
        }
    };
    if let Err(e) = run(cfg) {
        eprintln!("aprsmsg: {e}");
        process::exit(1);
    }
}
