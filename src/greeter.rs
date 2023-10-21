use std::{
  convert::TryInto,
  env,
  error::Error,
  fmt::{self, Display},
  path::PathBuf,
  process,
  sync::Arc,
};

use chrono::{
  format::{Item, StrftimeItems},
  Locale,
};
use getopts::{Matches, Options};
use i18n_embed::DesktopLanguageRequester;
use tokio::{
  net::UnixStream,
  process::Command,
  sync::{Notify, RwLock, RwLockWriteGuard},
};
use zeroize::Zeroize;

use crate::{
  info::{
    get_issue, get_last_session, get_last_session_path, get_last_user_name, get_last_user_session, get_last_user_session_path, get_last_user_username, get_min_max_uids, get_sessions, get_users,
  },
  power::PowerOption,
  ui::{
    common::menu::Menu,
    power::Power,
    sessions::{Session, SessionType},
    users::User,
  },
};

const DEFAULT_LOCALE: Locale = Locale::en_US;
const DEFAULT_ASTERISKS_CHAR: char = '*';
// `startx` wants an absolute path to the executable as a first argument.
// We don't want to resolve the session command in the greeter though, so it should be additionally wrapped with a known noop command (like `/usr/bin/env`).
const DEFAULT_XSESSION_WRAPPER: &str = "startx /usr/bin/env";

#[derive(Debug, Copy, Clone)]
pub enum AuthStatus {
  Success,
  Failure,
  Cancel,
}

impl Display for AuthStatus {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "{:?}", self)
  }
}

impl Error for AuthStatus {}

#[derive(SmartDefault, Debug, Copy, Clone, PartialEq)]
pub enum Mode {
  #[default]
  Username,
  Password,
  Users,
  Command,
  Sessions,
  Power,
  Processing,
}

#[derive(SmartDefault)]
pub struct Greeter {
  #[default(DEFAULT_LOCALE)]
  pub locale: Locale,
  pub config: Option<Matches>,
  pub socket: String,
  pub stream: Option<Arc<RwLock<UnixStream>>>,

  pub mode: Mode,
  pub previous_mode: Mode,
  pub cursor_offset: i16,

  pub users: Menu<User>,
  pub command: Option<String>,
  pub new_command: String,
  pub session_path: Option<PathBuf>,
  pub session_paths: Vec<(PathBuf, SessionType)>,
  pub sessions: Menu<Session>,
  pub xsession_wrapper: Option<String>,

  pub username: String,
  pub username_mask: Option<String>,
  pub prompt: Option<String>,
  pub answer: String,
  pub secret: bool,

  pub user_menu: bool,

  pub remember: bool,
  pub remember_session: bool,
  pub remember_user_session: bool,
  pub asterisks: bool,
  #[default(DEFAULT_ASTERISKS_CHAR)]
  pub asterisks_char: char,
  pub greeting: Option<String>,
  pub message: Option<String>,

  pub powers: Menu<Power>,
  pub power_command: Option<Command>,
  pub power_command_notify: Arc<Notify>,
  pub power_setsid: bool,

  pub working: bool,
  pub done: bool,
  pub exit: Option<AuthStatus>,
}

impl Drop for Greeter {
  fn drop(&mut self) {
    self.scrub(true, false);
  }
}

impl Greeter {
  pub async fn new() -> Self {
    let mut greeter = Self::default();

    greeter.set_locale();

    greeter.powers = Menu {
      title: fl!("title_power"),
      options: Default::default(),
      selected: 0,
    };

    greeter.parse_options().await;
    greeter.sessions = Menu {
      title: fl!("title_session"),
      options: get_sessions(&greeter).unwrap_or_default(),
      selected: 0,
    };

    if let Some(Session { command, .. }) = greeter.sessions.options.get(0) {
      if greeter.command.is_none() {
        greeter.command = Some(command.clone());
      }
    }

    if greeter.remember {
      if let Some(username) = get_last_user_username() {
        greeter.username = username.clone();
        greeter.username_mask = get_last_user_name();

        if greeter.remember_user_session {
          if let Ok(session_path) = get_last_user_session_path(&username) {
            greeter.session_path = Some(session_path);
          }

          if let Ok(command) = get_last_user_session(&username) {
            greeter.command = Some(command);
          }
        }
      }
    }

    if greeter.remember_session {
      if let Ok(session_path) = get_last_session_path() {
        greeter.session_path = Some(session_path);
      }

      if let Ok(command) = get_last_session() {
        greeter.command = Some(command.trim().to_string());
      }
    }

    greeter.sessions.selected = greeter
      .sessions
      .options
      .iter()
      .position(|Session { path, .. }| path.as_deref() == greeter.session_path.as_deref())
      .unwrap_or(0);

    greeter
  }

  fn scrub(&mut self, scrub_message: bool, soft: bool) {
    self.answer.zeroize();
    self.prompt.zeroize();

    if !soft {
      self.username.zeroize();
      self.username_mask.zeroize();
    }

    if scrub_message {
      self.message.zeroize();
    }
  }

  pub async fn reset(&mut self, soft: bool) {
    if soft {
      self.mode = Mode::Password;
      self.previous_mode = Mode::Password;
    } else {
      self.mode = Mode::Username;
      self.previous_mode = Mode::Username;
    }

    self.working = false;
    self.done = false;

    self.scrub(false, soft);
    self.connect().await;
  }

  pub async fn connect(&mut self) {
    match UnixStream::connect(&self.socket).await {
      Ok(stream) => self.stream = Some(Arc::new(RwLock::new(stream))),

      Err(err) => {
        eprintln!("{err}");
        process::exit(1);
      }
    }
  }

  pub fn config(&self) -> &Matches {
    self.config.as_ref().unwrap()
  }

  pub async fn stream(&self) -> RwLockWriteGuard<'_, UnixStream> {
    self.stream.as_ref().unwrap().write().await
  }

  pub fn option(&self, name: &str) -> Option<String> {
    self.config().opt_str(name)
  }

  pub fn width(&self) -> u16 {
    if let Some(value) = self.option("width") {
      if let Ok(width) = value.parse::<u16>() {
        return width;
      }
    }

    80
  }

  pub fn window_padding(&self) -> u16 {
    if let Some(value) = self.option("window-padding") {
      if let Ok(padding) = value.parse::<u16>() {
        return padding;
      }
    }

    0
  }

  pub fn container_padding(&self) -> u16 {
    if let Some(value) = self.option("container-padding") {
      if let Ok(padding) = value.parse::<u16>() {
        return padding + 1;
      }
    }

    2
  }

  pub fn prompt_padding(&self) -> u16 {
    if let Some(value) = self.option("prompt-padding") {
      if let Ok(padding) = value.parse::<u16>() {
        return padding;
      }
    }

    1
  }

  fn set_locale(&mut self) {
    let locale = DesktopLanguageRequester::requested_languages()
      .into_iter()
      .next()
      .and_then(|locale| locale.region.map(|region| format!("{}_{region}", locale.language)))
      .and_then(|id| id.as_str().try_into().ok());

    if let Some(locale) = locale {
      self.locale = locale;
    }
  }

  async fn parse_options(&mut self) {
    let mut opts = Options::new();

    let xsession_wrapper_desc = format!("wrapper command to initialize X server and launch X11 sessions (default: {DEFAULT_XSESSION_WRAPPER})");

    opts.optflag("h", "help", "show this usage information");
    opts.optflag("v", "version", "print version information");
    opts.optopt("c", "cmd", "command to run", "COMMAND");
    opts.optopt("s", "sessions", "colon-separated list of Wayland session paths", "DIRS");
    opts.optopt("x", "xsessions", "colon-separated list of X11 session paths", "DIRS");
    opts.optopt("", "xsession-wrapper", xsession_wrapper_desc.as_str(), "'CMD [ARGS]...'");
    opts.optflag("", "no-xsession-wrapper", "do not wrap commands for X11 sessions");
    opts.optopt("w", "width", "width of the main prompt (default: 80)", "WIDTH");
    opts.optflag("i", "issue", "show the host's issue file");
    opts.optopt("g", "greeting", "show custom text above login prompt", "GREETING");
    opts.optflag("t", "time", "display the current date and time");
    opts.optopt("", "time-format", "custom strftime format for displaying date and time", "FORMAT");
    opts.optflag("r", "remember", "remember last logged-in username");
    opts.optflag("", "remember-session", "remember last selected session");
    opts.optflag("", "remember-user-session", "remember last selected session for each user");
    opts.optflag("", "user-menu", "allow graphical selection of users from a menu");
    opts.optopt("", "user-menu-min-uid", "minimum UID to display in the user selection menu", "UID");
    opts.optopt("", "user-menu-max-uid", "maximum UID to display in the user selection menu", "UID");
    opts.optflag("", "asterisks", "display asterisks when a secret is typed");
    opts.optopt("", "asterisks-char", "character to be used to redact secrets (default: *)", "CHAR");
    opts.optopt("", "window-padding", "padding inside the terminal area (default: 0)", "PADDING");
    opts.optopt("", "container-padding", "padding inside the main prompt container (default: 1)", "PADDING");
    opts.optopt("", "prompt-padding", "padding between prompt rows (default: 1)", "PADDING");

    opts.optopt("", "power-shutdown", "command to run to shut down the system", "'CMD [ARGS]...'");
    opts.optopt("", "power-reboot", "command to run to reboot the system", "'CMD [ARGS]...'");
    opts.optflag("", "power-no-setsid", "do not prefix power commands with setsid");

    self.config = match opts.parse(env::args().collect::<Vec<String>>()) {
      Ok(matches) => Some(matches),

      Err(err) => {
        eprintln!("{err}");
        print_usage(opts);
        process::exit(1);
      }
    };

    if self.config().opt_present("help") {
      print_usage(opts);
      process::exit(0);
    }
    if self.config().opt_present("version") {
      print_version();
      process::exit(0);
    }

    match env::var("GREETD_SOCK") {
      Ok(socket) => self.socket = socket,
      Err(_) => {
        eprintln!("GREETD_SOCK must be defined");
        process::exit(1);
      }
    }

    if self.config().opt_present("issue") && self.config().opt_present("greeting") {
      eprintln!("Only one of --issue and --greeting may be used at the same time");
      print_usage(opts);
      process::exit(1);
    }

    if let Some(value) = self.config().opt_str("asterisks-char") {
      if value.chars().count() != 1 {
        eprintln!("--asterisks-char can only have one single character as its value");
        print_usage(opts);
        process::exit(1);
      }

      self.asterisks_char = value.chars().next().unwrap();
    }

    if let Some(format) = self.config().opt_str("time-format") {
      if StrftimeItems::new(&format).any(|item| item == Item::Error) {
        eprintln!("Invalid strftime format provided in --time-format");
        process::exit(1);
      }
    }

    if self.config().opt_present("user-menu") {
      self.user_menu = true;

      let min_uid = self.config().opt_str("user-menu-min-uid").and_then(|uid| uid.parse::<u16>().ok());
      let max_uid = self.config().opt_str("user-menu-max-uid").and_then(|uid| uid.parse::<u16>().ok());
      let (min_uid, max_uid) = get_min_max_uids(min_uid, max_uid);

      if min_uid >= max_uid {
        eprintln!("Minimum UID ({min_uid}) must be less than maximum UID ({max_uid})");
        process::exit(1);
      }

      self.users = Menu {
        title: fl!("title_users"),
        options: get_users(min_uid, max_uid),
        selected: 0,
      }
    }

    if self.config().opt_present("remember-session") && self.config().opt_present("remember-user-session") {
      eprintln!("Only one of --remember-session and --remember-user-session may be used at the same time");
      print_usage(opts);
      process::exit(1);
    }
    if self.config().opt_present("remember-user-session") && !self.config().opt_present("remember") {
      eprintln!("--remember-session must be used with --remember");
      print_usage(opts);
      process::exit(1);
    }

    self.remember = self.config().opt_present("remember");
    self.remember_session = self.config().opt_present("remember-session");
    self.remember_user_session = self.config().opt_present("remember-user-session");
    self.asterisks = self.config().opt_present("asterisks");
    self.greeting = self.option("greeting");
    self.command = self.option("cmd");

    if let Some(dirs) = self.option("sessions") {
      self.session_paths.extend(env::split_paths(&dirs).map(|dir| (dir, SessionType::Wayland)));
    }

    if let Some(dirs) = self.option("xsessions") {
      self.session_paths.extend(env::split_paths(&dirs).map(|dir| (dir, SessionType::X11)));
    }

    if !self.config().opt_present("no-xsession-wrapper") {
      self.xsession_wrapper = self.option("xsession-wrapper").or_else(|| Some(DEFAULT_XSESSION_WRAPPER.to_string()));
    }

    if self.config().opt_present("issue") {
      self.greeting = get_issue();
    }

    self.powers.options.push(Power {
      action: PowerOption::Shutdown,
      label: fl!("shutdown"),
      command: self.config().opt_str("power-shutdown"),
    });

    self.powers.options.push(Power {
      action: PowerOption::Reboot,
      label: fl!("reboot"),
      command: self.config().opt_str("power-reboot"),
    });

    self.power_setsid = !self.config().opt_present("power-no-setsid");

    self.connect().await;
  }

  pub fn set_prompt(&mut self, prompt: &str) {
    self.prompt = if prompt.ends_with(' ') { Some(prompt.into()) } else { Some(format!("{prompt} ")) };
  }

  pub fn remove_prompt(&mut self) {
    self.prompt = None;
  }

  pub fn prompt_width(&self) -> usize {
    match &self.prompt {
      None => 0,
      Some(prompt) => prompt.chars().count(),
    }
  }
}

fn print_usage(opts: Options) {
  eprint!("{}", opts.usage("Usage: tuigreet [OPTIONS]"));
}

fn print_version() {
  println!("tuigreet {} ({})", env!("VERSION"), env!("TARGET"));
  println!("Copyright (C) 2020 Antoine POPINEAU <https://github.com/apognu/tuigreet>.");
  println!("Licensed under GPLv3+ (GNU GPL version 3 or later).");
  println!();
  println!("This is free software, you are welcome to redistribute it under some conditions.");
  println!("There is NO WARRANTY, to the extent provided by law.");
}
