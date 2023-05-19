// SPDX-FileCopyrightText: 2021 Kent Gibson <warthog618@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::common::{
    self, ActiveLowOpts, BiasOpts, ChipInfo, DriveOpts, LineOpts, ParseDurationError, UapiOpts,
};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Arg, ArgAction, Command, Parser};
use daemonize::Daemonize;
use gpiocdev::line::{Offset, Value, Values};
use gpiocdev::request::{Config, Request};
use rustyline::completion::{Completer, Pair};
use rustyline::config::CompletionType;
use rustyline::error::ReadlineError;
use rustyline::Editor;
use rustyline_derive::{Helper, Highlighter, Hinter, Validator};
use std::cmp;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(alias("s"))]
pub struct Opts {
    /// The line values.
    ///
    /// The values are specified in name=value format or optionally in offset=value
    /// format if the --chip option is provided.
    ///
    /// Values may be inactive/off/false/0 or active/on/true/1.
    /// e.g.
    ///     GPIO17=on GPIO22=inactive
    ///     --chip gpiochip0 17=1 22=0
    #[arg(name = "line=value", required = true, value_parser = parse_line_value, verbatim_doc_comment)]
    line_values: Vec<(String, LineValue)>,

    /// Display a banner on successful startup
    #[arg(long)]
    banner: bool,

    #[command(flatten)]
    line_opts: LineOpts,

    #[command(flatten)]
    active_low_opts: ActiveLowOpts,

    #[command(flatten)]
    bias_opts: BiasOpts,

    #[command(flatten)]
    drive_opts: DriveOpts,

    /// Set the lines then wait for additional set commands for the requested lines.
    ///
    /// Use the "help" command at the interactive prompt to get help for
    /// the supported commands.
    #[arg(short, long, groups = ["mode", "terminal"])]
    interactive: bool,

    /// The minimum time period to hold lines at the requested values.
    ///
    /// The period is taken as milliseconds unless otherwise specified.
    #[arg(short = 'p', long, name = "period", value_parser = common::parse_duration)]
    hold_period: Option<Duration>,

    /// Toggle the lines after the specified time periods.
    ///
    /// The time periods are a comma separated list, and are taken as
    /// milliseconds unless otherwise specified.
    /// The lines are toggled after the period elapses, so the initial time period
    /// applies to the initial line value.
    /// If the final period is not zero, then the sequence repeats.
    ///
    ///  e.g.
    ///      -t 10ms
    ///      -t 100us,200us,100us,150us
    ///      -t 1s,2s,1s,0
    ///
    /// The first two examples repeat, the third exits after 4s.
    ///
    /// A 0s period elsewhere in the sequence is toggled as quickly as possible,
    /// allowing for any specified --hold-period.
    #[arg(short = 't', long, name = "periods", value_parser = parse_time_sequence, group = "mode", verbatim_doc_comment)]
    toggle: Option<TimeSequence>,

    /// Set line values then detach from the controlling terminal.
    #[arg(short = 'z', long, group = "terminal")]
    daemonize: bool,

    /// The consumer label applied to requested lines.
    #[arg(short = 'C', long, name = "name", default_value = "gpiocdev-set")]
    consumer: String,

    #[command(flatten)]
    uapi_opts: UapiOpts,
}

impl Opts {
    // mutate the config to match the configuration
    fn apply(&self, config: &mut Config) {
        self.active_low_opts.apply(config);
        self.bias_opts.apply(config);
        self.drive_opts.apply(config);
    }
}

pub fn cmd(opts: &Opts) -> Result<()> {
    let mut setter = Setter {
        hold_period: opts.hold_period,
        ..Default::default()
    };
    setter.request(opts)?;
    if opts.banner {
        let line_ids: Vec<String> = opts
            .line_values
            .iter()
            .map(|(l, _v)| l.to_owned())
            .collect();
        print_banner(&line_ids);
    }
    if opts.daemonize {
        Daemonize::new().start()?;
    }
    if let Some(ts) = &opts.toggle {
        return setter.toggle(ts);
    }
    setter.hold();
    if opts.interactive {
        return setter.interact(opts);
    }
    setter.wait();
    Ok(())
}

#[derive(Default)]
struct Setter {
    // IDs of requested lines - in command line order
    line_ids: Vec<String>,

    // Map from command line name to top level line details
    lines: HashMap<String, Line>,

    // The list of chips containing requested lines
    chips: Vec<ChipInfo>,

    // The request on each chip
    requests: Vec<Request>,

    // The minimum period to hold set values before applying the subsequent set
    hold_period: Option<Duration>,

    // Flag indicating if last operation resulted in a hold
    last_held: bool,
}

impl Setter {
    fn request(&mut self, opts: &Opts) -> Result<()> {
        self.line_ids = opts
            .line_values
            .iter()
            .map(|(l, _v)| l.to_owned())
            .collect();
        let abiv = common::actual_abi_version(&opts.uapi_opts)?;
        let r = common::resolve_lines(&self.line_ids, &opts.line_opts, abiv)?;
        r.validate(&self.line_ids, &opts.line_opts)?;
        self.chips = r.chips;

        // find set of lines for each chip
        for (id, v) in &opts.line_values {
            let co = r.lines.get(id).unwrap();
            self.lines.insert(
                id.to_owned(),
                Line {
                    chip_idx: co.chip_idx,
                    offset: co.offset,
                    value: v.0,
                    dirty: false,
                },
            );
        }

        // request the lines
        for (idx, ci) in self.chips.iter().enumerate() {
            let mut cfg = Config::default();
            opts.apply(&mut cfg);
            for line in self.lines.values() {
                if line.chip_idx == idx {
                    cfg.with_line(line.offset).as_output(line.value);
                }
            }
            let mut bld = Request::from_config(cfg);
            bld.on_chip(&ci.path).with_consumer(&opts.consumer);
            #[cfg(all(feature = "uapi_v1", feature = "uapi_v2"))]
            bld.using_abi_version(abiv);
            let req = bld
                .request()
                .with_context(|| format!("failed to request and set lines on {}", ci.name))?;
            self.requests.push(req);
        }
        Ok(())
    }

    fn interact(&mut self, opts: &Opts) -> Result<()> {
        use std::io::Write;

        let helper = InteractiveHelper {
            line_names: opts
                .line_values
                .iter()
                .map(|(l, _v)| l.to_owned())
                .collect(),
        };
        let config = rustyline::Config::builder()
            .completion_type(CompletionType::List)
            .auto_add_history(true)
            .max_history_size(20)?
            .history_ignore_space(true)
            .build();
        let mut rl = Editor::with_config(config)?;
        rl.set_helper(Some(helper));
        let mut stdout = std::io::stdout();
        let prompt = "gpiocdev-set> ";
        let cmd = Command::new("gpiocdev")
            .no_binary_name(true)
            .disable_help_flag(true)
            .infer_subcommands(true)
            .override_help(interactive_help())
            .subcommand(
                Command::new("get")
                    .about("Display the current values of the given requested lines")
                    .arg(
                        Arg::new("lines")
                            .required(false)
                            .action(ArgAction::Append)
                            .value_parser(parse_line),
                    ),
            )
            .subcommand(
                Command::new("set")
                    .about("Update the values of the given requested lines")
                    .help_template("{name} woot {about}")
                    .arg(
                        Arg::new("line_values")
                            .value_name("line=value")
                            .required(true)
                            .action(ArgAction::Append)
                            .value_parser(parse_line_value),
                    ),
            )
            .subcommand(
                Command::new("sleep")
                    .about("Sleep for the specified period")
                    .arg(
                        Arg::new("duration")
                            .required(true)
                            .action(ArgAction::Set)
                            .value_parser(common::parse_duration),
                    ),
            )
            .subcommand(
                Command::new("toggle")
                    .about(
                        "Toggle the values of the given requested lines\n\
            If no lines are specified then all requested lines are toggled.",
                    )
                    .arg(
                        Arg::new("lines")
                            .required(false)
                            .action(ArgAction::Append)
                            .value_parser(parse_line),
                    ),
            )
            .subcommand(Command::new("exit").about("Exit the program"));
        loop {
            /*
             * manually print the prompt, as rustyline doesn't if stdout
             * is not a tty? And flush to ensure the prompt and any
             * output buffered from the previous command is sent.
             */
            _ = stdout.write(prompt.as_bytes());
            _ = stdout.flush();
            let readline = rl.readline(prompt);
            match readline {
                Ok(line) => {
                    match self.parse_command(cmd.clone(), &line) {
                        Err(err) if err.is::<ExitCmdError>() => return Ok(()),
                        Err(err) => {
                            println!("{}", err);
                            // clean in case the error leaves dirty lines.
                            self.clean();
                        }
                        Ok(_) => {}
                    }
                }
                Err(ReadlineError::Interrupted) => return Ok(()),
                Err(ReadlineError::Eof) => return Ok(()),
                Err(err) => bail!(err),
            }
        }
    }

    fn parse_command(&mut self, cmd: Command, line: &str) -> Result<()> {
        let mut words = CommandWords::new(line);
        let mut args = Vec::new();
        while let Some(word) = &words.next() {
            args.push(*word);
        }
        if words.inquote {
            return Err(anyhow!(format!(
                "missing closing quote in '{}'",
                args.last().unwrap()
            )));
        }
        match cmd.try_get_matches_from(args) {
            Ok(opt) => match opt.subcommand() {
                Some(("exit", _)) => Err(anyhow!(ExitCmdError {})),
                Some(("get", am)) => {
                    let lines: Vec<String> = am
                        .get_many::<String>("lines")
                        .unwrap_or_default()
                        .cloned()
                        .collect();
                    self.do_get(lines.as_slice())
                }
                Some(("set", am)) => {
                    let lvs: Vec<(String, LineValue)> = am
                        .get_many::<(String, LineValue)>("line_values")
                        .unwrap()
                        .cloned()
                        .collect();
                    self.do_set(lvs.as_slice())
                }
                Some(("sleep", am)) => {
                    let d: Duration = am.get_one::<Duration>("duration").unwrap().to_owned();
                    self.do_sleep(d)
                }
                Some(("toggle", am)) => {
                    let lines: Vec<String> = am
                        .get_many::<String>("lines")
                        .unwrap_or_default()
                        .cloned()
                        .collect();
                    self.do_toggle(lines.as_slice())
                }
                Some((&_, _)) => Ok(()),
                None => Ok(()),
            },
            Err(e) => Err(anyhow!(e)),
        }
    }

    fn hold(&mut self) {
        if let Some(period) = self.hold_period {
            self.last_held = true;
            thread::sleep(period);
        }
    }

    fn do_get(&mut self, lines: &[String]) -> Result<()> {
        let mut print_values = Vec::new();
        for id in lines {
            match self.lines.get(id) {
                Some(line) => {
                    print_values.push(if !id.contains(' ') {
                        format!("{}={}", id, line.value)
                    } else {
                        format!("\"{}\"={}", id, line.value)
                    });
                }
                None => bail!("not a requested line: '{}'", id),
            }
        }
        if print_values.is_empty() {
            // no lines specified, so return all lines
            for id in &self.line_ids {
                let value = self.lines.get(id).unwrap().value;
                print_values.push(if !id.contains(' ') {
                    format!("{}={}", id, value)
                } else {
                    format!("\"{}\"={}", id, value)
                });
            }
        }
        println!("{}", print_values.join(" "));

        Ok(())
    }

    fn do_set(&mut self, changes: &[(String, LineValue)]) -> Result<()> {
        for (id, value) in changes {
            match self.lines.get_mut(id) {
                Some(line) => {
                    line.value = value.0;
                    line.dirty = true;
                }
                None => bail!("not a requested line: '{}'", id),
            }
        }
        if self.update()? {
            self.hold();
        }
        Ok(())
    }

    fn do_sleep(&mut self, mut d: Duration) -> Result<()> {
        if self.last_held {
            self.last_held = false;
            if let Some(period) = self.hold_period {
                if d < period {
                    // slept longer than that already
                    return Ok(());
                }
                d -= period;
            }
        }
        thread::sleep(d);
        Ok(())
    }

    fn do_toggle(&mut self, lines: &[String]) -> Result<()> {
        for id in lines {
            match self.lines.get_mut(id) {
                Some(line) => {
                    line.value = line.value.not();
                    line.dirty = true;
                }
                None => bail!("not a requested line: '{}'", id),
            }
        }
        if lines.is_empty() {
            // no lines specified, so toggle all lines
            self.toggle_all_lines();
        }
        if self.update()? {
            self.hold();
        }
        Ok(())
    }

    fn clean(&mut self) {
        for line in self.lines.values_mut() {
            line.dirty = false;
        }
    }

    fn toggle(&mut self, ts: &TimeSequence) -> Result<()> {
        if ts.0.len() == 1 && ts.0[0].is_zero() {
            self.hold();
            return Ok(());
        }
        let mut count = 0;
        let hold_period = self.hold_period.unwrap_or(Duration::ZERO);
        loop {
            thread::sleep(cmp::max(ts.0[count], hold_period));
            count += 1;
            if count == ts.0.len() - 1 && ts.0[count].is_zero() {
                return Ok(());
            }
            if count == ts.0.len() {
                count = 0;
            }
            self.toggle_all_lines();
            self.update()?;
        }
    }

    fn toggle_all_lines(&mut self) {
        for line in self.lines.values_mut() {
            line.value = line.value.not();
            line.dirty = true;
        }
    }

    fn update(&mut self) -> Result<bool> {
        let mut updated = false;
        for idx in 0..self.chips.len() {
            let mut values = Values::default();
            for line in self.lines.values_mut() {
                if line.dirty && line.chip_idx == idx {
                    values.set(line.offset, line.value);
                    line.dirty = false;
                }
            }
            if !values.is_empty() {
                self.requests[idx]
                    .set_values(&values)
                    .context("set failed:")?;
                updated = true;
            }
        }
        Ok(updated)
    }

    fn wait(&self) {
        // just block on something that should never happen...
        _ = self.requests[0].read_edge_event();
    }
}

#[derive(Debug)]
struct ExitCmdError {}

impl fmt::Display for ExitCmdError {
    fn fmt(&self, _f: &mut fmt::Formatter) -> fmt::Result {
        Ok(())
    }
}
impl Error for ExitCmdError {}

fn interactive_help() -> String {
    let mut help = "COMMANDS:\n".to_owned();

    let cmds = [
        (
            "get [line]...",
            "Display the current values of the given requested lines",
        ),
        (
            "set <line=value>...",
            "Update the values of the given requested lines",
        ),
        (
            "toggle [line]...",
            "Toggle the values of the given requested lines\n\
            If no lines are specified then all requested lines are toggled.",
        ),
        ("sleep <period>", "Sleep for the specified period"),
        ("help", "Print this help"),
        ("exit", "Exit the program"),
    ];
    for (cmd, desc) in cmds {
        let cmd_line = format!("\n    {}", cmd);
        help.push_str(&cmd_line);
        for line in desc.split('\n') {
            let desc_line = format!("\n            {}\n", line);
            help.push_str(&desc_line);
        }
    }
    help
}

fn print_banner(lines: &[String]) {
    use std::io::Write;
    if lines.len() > 1 {
        print!("Setting lines ");

        for l in lines.iter().take(lines.len() - 1) {
            print!("'{}', ", l);
        }

        println!("and '{}'...", lines[lines.len() - 1]);
    } else {
        println!("Setting line '{}'...", lines[0]);
    }
    _ = std::io::stdout().flush();
}

#[derive(Debug, Default)]
struct Line {
    chip_idx: usize,
    offset: Offset,
    value: Value,
    dirty: bool,
}

// strips quotes surrounding the whole string.
fn unquoted(s: &str) -> &str {
    if s.starts_with('"') && s.ends_with('"') && s.len() > 1 {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse a single line id
fn parse_line(s: &str) -> std::result::Result<String, Box<dyn Error + Send + Sync + 'static>> {
    Ok(unquoted(s).to_string())
}

/// Parse a single line=value pair
fn parse_line_value(s: &str) -> std::result::Result<(String, LineValue), anyhow::Error> {
    let pos = s
        .rfind('=')
        .ok_or_else(|| anyhow!("invalid line=value: no '=' found in '{}'", s))?;
    let ln = unquoted(&s[..pos]);
    if ln.contains('"') {
        bail!("invalid line=value: semi-quoted line name in '{}'", s)
    } else {
        Ok((ln.to_string(), s[pos + 1..].parse()?))
    }
}

#[derive(Clone, Debug)]
struct TimeSequence(Vec<Duration>);

fn parse_time_sequence(s: &str) -> std::result::Result<TimeSequence, ParseDurationError> {
    let mut ts = TimeSequence(Vec::new());
    for period in s.split(',') {
        ts.0.push(common::parse_duration(period)?);
    }
    Ok(ts)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LineValue(Value);

impl FromStr for LineValue {
    type Err = InvalidLineValue;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let lower_s = s.to_lowercase();
        let v = match lower_s.as_str() {
            "0" | "inactive" | "off" | "false" => Value::Inactive,
            "1" | "active" | "on" | "true" => Value::Active,
            _ => {
                return Err(InvalidLineValue::new(s));
            }
        };
        Ok(LineValue(v))
    }
}

#[derive(Debug)]
struct InvalidLineValue {
    value: String,
}

impl InvalidLineValue {
    pub fn new<S: Into<String>>(value: S) -> InvalidLineValue {
        InvalidLineValue {
            value: value.into(),
        }
    }
}

impl fmt::Display for InvalidLineValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid line value: '{}'", self.value)
    }
}
impl Error for InvalidLineValue {}

#[derive(Helper, Validator, Hinter, Highlighter)]
struct InteractiveHelper {
    line_names: Vec<String>,
}

impl InteractiveHelper {
    fn complete_set(&self, mut pos: usize, mut words: CommandWords) -> (usize, Vec<Pair>) {
        let mut candidates = Vec::new();
        let mut line_values = Vec::new();
        while let Some(word) = &words.next() {
            line_values.push(*word);
        }
        let selected: Vec<&'_ str> = line_values
            .iter()
            .filter(|lv| lv.contains('='))
            .map(|lv| unquoted(&lv[..lv.find('=').unwrap()]))
            .collect();
        let unselected = self
            .line_names
            .iter()
            .filter(|l| !selected.contains(&l.as_str()));
        if !words.partial {
            for line in unselected {
                candidates.push(line_value_pair(line));
            }
            return (pos, candidates);
        }
        let mut part_word = *line_values.last().unwrap();
        match part_word.split_once('=') {
            Some((_, part_value)) => {
                const VALUES: [&str; 8] =
                    ["active", "inactive", "on", "off", "true", "false", "1", "0"];
                pos -= part_value.len();
                for value in VALUES.iter().filter(|v| v.starts_with(part_value)) {
                    candidates.push(base_pair(value))
                }
            }
            None => {
                pos -= part_word.len();
                part_word = unquoted(part_word);
                if part_word.starts_with('"') {
                    part_word = &part_word[1..];
                }
                for line in unselected.filter(|l| l.starts_with(part_word)) {
                    candidates.push(line_value_pair(line))
                }
            }
        }
        (pos, candidates)
    }

    fn complete_sleep(&self, mut pos: usize, mut words: CommandWords) -> (usize, Vec<Pair>) {
        const UNITS: [&str; 4] = ["s", "ms", "us", "ns"];
        let mut candidates = Vec::new();
        let mut times = Vec::new();
        while let Some(word) = &words.next() {
            times.push(*word);
        }
        if words.partial && times.len() == 1 {
            let t = &times[0];
            match t.find(|c: char| !c.is_ascii_digit()) {
                Some(n) => {
                    let (_num, units) = t.split_at(n);
                    pos -= units.len();
                    for display in UNITS.iter().filter(|u| u.starts_with(units)) {
                        candidates.push(base_pair(display))
                    }
                }
                None => {
                    for display in UNITS.iter() {
                        candidates.push(base_pair(display))
                    }
                }
            }
        }
        (pos, candidates)
    }

    fn complete_lines(&self, pos: usize, mut words: CommandWords) -> (usize, Vec<Pair>) {
        let mut selected = Vec::new();
        while let Some(word) = &words.next() {
            selected.push(unquoted(word));
        }
        let unselected = self
            .line_names
            .iter()
            .filter(|l| !selected.contains(&l.as_str()));
        if !words.partial {
            let candidates = unselected.map(|l| line_pair(l)).collect();
            return (pos, candidates);
        }
        let mut part_word = *selected.last().unwrap();
        let lpos = pos - part_word.len();
        if part_word.starts_with('"') {
            part_word = &part_word[1..];
        }
        let candidates = unselected
            .filter(|l| l.starts_with(part_word))
            .map(|l| line_pair(l))
            .collect();
        (lpos, candidates)
    }
}

impl Completer for InteractiveHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> Result<(usize, Vec<Pair>), ReadlineError> {
        const CMD_SET: [&str; 6] = ["exit", "get", "help", "set", "sleep", "toggle"];
        let cmd_pos = line.len() - line.trim_start().len();
        let mut words = CommandWords::new(&line[cmd_pos..pos]);
        Ok(match words.next() {
            Some(cmd) => {
                if words.partial {
                    let mut candidates = Vec::new();
                    for display in CMD_SET.iter().filter(|x| x.starts_with(cmd)) {
                        candidates.push(base_pair(display))
                    }
                    (cmd_pos, candidates)
                } else {
                    match cmd {
                        "get" => self.complete_lines(pos, words),
                        "set" => self.complete_set(pos, words),
                        "sleep" => self.complete_sleep(pos, words),
                        "toggle" => self.complete_lines(pos, words),
                        _ => (cmd_pos, vec![]),
                    }
                }
            }
            None => {
                let mut candidates = Vec::new();
                for display in CMD_SET.iter() {
                    candidates.push(base_pair(display))
                }
                (0, candidates)
            }
        })
    }
}

// a pair that ends a command word
fn base_pair(candidate: &str) -> Pair {
    let display = String::from(candidate);
    let mut replacement = display.clone();
    replacement.push(' ');
    Pair {
        display,
        replacement,
    }
}

fn quotable(line: &str) -> String {
    // force quotes iff necessary
    if line.contains(' ') {
        let mut quoted = "\"".to_string();
        quoted.push_str(line);
        quoted.push('"');
        quoted
    } else {
        line.to_string()
    }
}

// a pair that contains a line name - that may need to be quoted.
fn line_pair(candidate: &str) -> Pair {
    let display = String::from(candidate);
    let mut replacement = quotable(candidate);
    replacement.push(' ');
    Pair {
        display,
        replacement,
    }
}

// a pair that contains a line name - that may need to be quoted,
// and will be followed by a value.
fn line_value_pair(candidate: &str) -> Pair {
    let display = String::from(candidate);
    let mut replacement = quotable(candidate);
    replacement.push('=');
    Pair {
        display,
        replacement,
    }
}

struct CommandWords<'a> {
    line: &'a str,
    liter: std::str::CharIndices<'a>,
    inquote: bool,
    partial: bool,
}

impl CommandWords<'_> {
    fn new(line: &str) -> CommandWords {
        CommandWords {
            line,
            liter: line.char_indices(),
            inquote: false,
            partial: false,
        }
    }
}

impl<'a> Iterator for CommandWords<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let start;
        loop {
            match self.liter.next() {
                Some((_, ' ')) => {}
                Some((pos, c)) => {
                    start = pos;
                    if c == '"' {
                        self.inquote = true;
                    }
                    break;
                }
                None => return None,
            }
        }
        loop {
            match self.liter.next() {
                Some((_, '"')) if self.inquote => self.inquote = false,
                Some((_, '"')) if !self.inquote => self.inquote = true,
                Some((pos, ' ')) if !self.inquote => {
                    return Some(&self.line[start..pos]);
                }
                Some((_, _)) => {}
                None => {
                    self.partial = true;
                    return Some(&self.line[start..]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse {
        #[test]
        fn line() {
            use super::parse_line;
            assert_eq!(parse_line("unquoted").unwrap(), "unquoted".to_string());
            assert_eq!(parse_line("\"semi").unwrap(), "\"semi".to_string());
            assert_eq!(parse_line("\"quoted\"").unwrap(), "quoted".to_string());
        }

        #[test]
        fn line_value() {
            use super::{parse_line_value, LineValue};
            use gpiocdev::line::Value;
            assert_eq!(
                parse_line_value("blah=0").unwrap(),
                ("blah".to_string(), LineValue(Value::Inactive))
            );
            assert_eq!(
                parse_line_value("l=1").unwrap(),
                ("l".to_string(), LineValue(Value::Active))
            );
            assert_eq!(
                parse_line_value("l=active").unwrap(),
                ("l".to_string(), LineValue(Value::Active))
            );
            assert_eq!(
                parse_line_value("l=inactive").unwrap(),
                ("l".to_string(), LineValue(Value::Inactive))
            );
            assert_eq!(
                parse_line_value("l=on").unwrap(),
                ("l".to_string(), LineValue(Value::Active))
            );
            assert_eq!(
                parse_line_value("l=off").unwrap(),
                ("l".to_string(), LineValue(Value::Inactive))
            );
            assert_eq!(
                parse_line_value("l=true").unwrap(),
                ("l".to_string(), LineValue(Value::Active))
            );
            assert_eq!(
                parse_line_value("l=false").unwrap(),
                ("l".to_string(), LineValue(Value::Inactive))
            );
            assert_eq!(
                parse_line_value("\"quoted\"=false").unwrap(),
                ("quoted".to_string(), LineValue(Value::Inactive))
            );
            assert_eq!(
                parse_line_value("\"quoted\\ name\"=1").unwrap(),
                ("quoted\\ name".to_string(), LineValue(Value::Active))
            );
            assert_eq!(
                parse_line_value("\"quoted=false")
                    .err()
                    .unwrap()
                    .to_string(),
                "invalid line=value: semi-quoted line name in '\"quoted=false'"
            );
            assert_eq!(
                parse_line_value("unquoted\"=false")
                    .err()
                    .unwrap()
                    .to_string(),
                "invalid line=value: semi-quoted line name in 'unquoted\"=false'"
            );
            assert_eq!(
                parse_line_value("5").err().unwrap().to_string(),
                "invalid line=value: no '=' found in '5'"
            );
            assert_eq!(
                parse_line_value("blah=3").err().unwrap().to_string(),
                "invalid line value: '3'"
            );
        }

        #[test]
        fn time_sequence() {
            use super::parse_time_sequence;
            use crate::common::ParseDurationError;
            use std::time::Duration;
            assert!(parse_time_sequence("0")
                .unwrap()
                .0
                .iter()
                .eq(vec![Duration::ZERO].iter()));
            assert!(parse_time_sequence("1")
                .unwrap()
                .0
                .iter()
                .eq(vec![Duration::from_millis(1)].iter()));
            assert!(parse_time_sequence("2ms")
                .unwrap()
                .0
                .iter()
                .eq(vec![Duration::from_millis(2)].iter()));
            assert!(parse_time_sequence("3us")
                .unwrap()
                .0
                .iter()
                .eq(vec![Duration::from_micros(3)].iter()));
            assert!(parse_time_sequence("4s")
                .unwrap()
                .0
                .iter()
                .eq(vec![Duration::new(4, 0)].iter()));
            assert!(parse_time_sequence("1,2ms,3us,4s,0")
                .unwrap()
                .0
                .iter()
                .eq(vec![
                    Duration::from_millis(1),
                    Duration::from_millis(2),
                    Duration::from_micros(3),
                    Duration::new(4, 0),
                    Duration::ZERO
                ]
                .iter()));
            assert_eq!(
                parse_time_sequence("5ns").unwrap_err(),
                ParseDurationError::Units("5ns".to_string())
            );
            assert_eq!(
                parse_time_sequence("bad").unwrap_err(),
                ParseDurationError::NoDigits("bad".to_string())
            );
        }
    }

    mod command_words {
        use super::CommandWords;
        #[test]
        fn whole_words() {
            let mut words = CommandWords::new("basic command line");
            let mut word = words.next().unwrap();
            assert_eq!(word, "basic");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "command");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "line");
            assert!(words.partial);
            assert!(!words.inquote);
            assert_eq!(words.next(), None);
            assert!(words.partial);
            assert!(!words.inquote);
        }

        #[test]
        fn quoted_words() {
            let mut words = CommandWords::new("quoted \"command lines\" \"are awful");
            let mut word = words.next().unwrap();
            assert_eq!(word, "quoted");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "\"command lines\"");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "\"are awful");
            assert!(words.partial);
            assert!(words.inquote);
            assert_eq!(words.next(), None);
            assert!(words.partial);
            assert!(words.inquote);
        }

        #[test]
        fn quotes_mid_words() {
            let mut words = CommandWords::new("quoted \"comm\"and\" lines\" \"are awful");
            let mut word = words.next().unwrap();
            assert_eq!(word, "quoted");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "\"comm\"and\" lines\"");
            assert!(!words.partial);
            assert!(!words.inquote);
            word = words.next().unwrap();
            assert_eq!(word, "\"are awful");
            assert!(words.partial);
            assert!(words.inquote);
            assert_eq!(words.next(), None);
            assert!(words.partial);
            assert!(words.inquote);
        }
    }
}
