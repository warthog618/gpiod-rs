// SPDX-FileCopyrightText: 2021 Kent Gibson <warthog618@gmail.com>
//
// SPDX-License-Identifier: MIT

use super::common::{self, LineOpts, UapiOpts};
use anyhow::{Context, Result};
use clap::Parser;
use gpiocdev::chip::Chip;
use gpiocdev::line::{Info, Offset};
use std::path::Path;

#[derive(Debug, Parser)]
#[command(aliases(["l", "info"]))]
pub struct Opts {
    /// Only get information for the specified lines
    ///
    /// The lines are identified by name or optionally by offset
    /// if the --chip option is provided.
    ///
    /// If not specified then all lines are returned.
    #[arg(name = "line")]
    lines: Vec<String>,

    /// Restrict scope to the lines on this chip
    ///
    /// If not specified then the scope is all chips in the system.
    ///
    /// If specified then lines may be identified by either name or offset.
    ///
    /// The chip may be identified by number, name, or path.
    /// e.g. the following all select the same chip:
    ///     -c 0
    ///     -c gpiochip0
    ///     -c /dev/gpiochip0
    #[arg(short, long, name = "chip", verbatim_doc_comment)]
    chip: Option<String>,

    /// Lines are strictly identified by name
    ///
    /// If --chip is provided then lines are initially assumed to be offsets, and only
    /// fallback to names if the line does not parse as an offset.
    ///
    /// With --by-name set the lines are never assumed to be identified by offsets, only names.
    #[arg(long)]
    by_name: bool,

    /// Check all lines - don't assume names are unique
    ///
    /// If not specified then the command stops when a matching line is found.
    ///
    /// If specified then all lines with the specified name are returned,
    /// each on a separate line.
    #[arg(short = 's', long)]
    strict: bool,

    #[arg(from_global)]
    pub verbose: bool,

    #[command(flatten)]
    uapi_opts: UapiOpts,
}

pub fn cmd(opts: &Opts) -> Result<bool> {
    let mut success = true;

    if !(opts.lines.is_empty() || opts.strict) {
        return print_first_matching_lines(opts);
    }

    let chips = if let Some(id) = &opts.chip {
        vec![common::chip_lookup_from_id(id)?]
    } else {
        common::all_chip_paths()?
    };
    if opts.lines.is_empty() {
        for p in chips {
            if !print_chip_all_lines(&p, opts)? {
                success = false;
            }
        }
    } else {
        let mut counts = vec![0; opts.lines.len()];
        for p in chips {
            print_chip_matching_lines(&p, opts, &mut counts)?;
        }
        success = counts.iter().all(|n| *n == 1);
    }
    Ok(success)
}

fn print_chip_all_lines(p: &Path, opts: &Opts) -> Result<bool> {
    match common::chip_from_path(p, opts.uapi_opts.abiv) {
        Ok(c) => {
            let kci = c
                .info()
                .with_context(|| format!("unable to read info from {}", c.name()))?;
            println!(
                "{} - {} lines:",
                common::string_or_default(&kci.name, "??"),
                kci.num_lines
            );
            for offset in 0..kci.num_lines {
                let li = c.line_info(offset).with_context(|| {
                    format!("unable to read line {} info from {}.", offset, c.name())
                })?;
                println!(
                    "\tline {:>3}:\t{:16}\t{}",
                    li.offset,
                    common::string_or_default(&li.name, "unnamed"),
                    common::stringify_attrs(&li),
                );
            }
            return Ok(true);
        }
        Err(e) if opts.chip.is_some() => return Err(e),
        Err(e) if opts.verbose => eprintln!("{:#}", e),
        Err(_) => {}
    }
    Ok(false)
}

fn print_chip_matching_lines(p: &Path, opts: &Opts, counts: &mut [u32]) -> Result<bool> {
    match common::chip_from_path(p, opts.uapi_opts.abiv) {
        Ok(c) => {
            let kci = c
                .info()
                .with_context(|| format!("unable to read info from {}", c.name()))?;
            for offset in 0..kci.num_lines {
                let li = c.line_info(offset).with_context(|| {
                    format!("unable to read info for line {} from {}", offset, c.name())
                })?;
                let oname = format!("{}", offset);
                for (idx, id) in opts.lines.iter().enumerate() {
                    if (id == &li.name) || (!opts.by_name && opts.chip.is_some() && id == &oname) {
                        counts[idx] += 1;
                        print_line_info(&kci.name, &li);
                    }
                }
            }
            return Ok(true);
        }
        Err(e) if opts.chip.is_some() => return Err(e),
        Err(e) if opts.verbose => eprintln!("{:#}", e),
        Err(_) => {}
    }
    Ok(false)
}

fn print_first_matching_lines(opts: &Opts) -> Result<bool> {
    let line_opts = LineOpts {
        chip: opts.chip.clone(),
        strict: false,
        by_name: opts.by_name,
    };
    let r = common::resolve_lines(&opts.lines, &line_opts, opts.uapi_opts.abiv)?;
    for (idx, ci) in r.chips.iter().enumerate() {
        let mut offsets: Vec<Offset> = r
            .lines
            .values()
            .filter(|co| co.chip_idx == idx)
            .map(|co| co.offset)
            .collect();
        offsets.sort_unstable();
        offsets.dedup();
        let mut c = common::chip_from_path(&ci.path, opts.uapi_opts.abiv)?;
        print_chip_line_info(&mut c, &offsets)?;
    }
    Ok(true)
}

fn print_chip_line_info(chip: &mut Chip, lines: &[Offset]) -> Result<()> {
    for &offset in lines {
        let li = chip
            .line_info(offset)
            .with_context(|| format!("unable to read line {} info from {}", offset, chip.name()))?;
        print_line_info(&chip.name(), &li);
    }
    Ok(())
}

fn print_line_info(chip_name: &str, li: &Info) {
    println!(
        "{} {}\t{:16}\t{}",
        common::string_or_default(chip_name, "??"),
        li.offset,
        common::string_or_default(&li.name, "unnamed"),
        common::stringify_attrs(li),
    );
}