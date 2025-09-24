use clap::{Arg, Command as ClapCommand};
use notify_rust::{Hint, Notification, Timeout};
use regex::Regex;
use serde::Deserialize;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const APP_NAME: &str = "x11-primary-switcher";
const APP_SUMMARY: &str = "X11 Primary Monitor Switcher";

fn main() {
    let matches = ClapCommand::new(APP_NAME)
        .version("1.0")
        .about("Switch X11 (XWayland) primary monitor with interactive or automatic modes, using Sway config hints.")
        .arg(
            Arg::new("auto-switch")
                .long("auto-switch")
                .help("Cycle the primary to the next connected X11 output")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("default")
                .long("default")
                .help("Set primary to the monitor indicated by the Sway config Primary Monitor block")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("status")
                .long("status")
                .help("Print the current primary X11 output and exit")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .value_name("PATH")
                .help("Path to Sway config (default: ~/.config/sway/config)"),
        )
        .get_matches();

    // Ensure xrandr is usable (i.e., XWayland is running)
    if !cmd_ok("xrandr", &["--query"]) {
        notify_error(
            "xrandr --query failed. Are you in a Wayland session with XWayland? Is xrandr installed?",
        );
        eprintln!("Error: xrandr --query failed.");
        std::process::exit(1);
    }

    // Read current X11 outputs
    let mut outputs = match xrandr_list_outputs() {
        Ok(v) if !v.is_empty() => v,
        _ => {
            notify_error("No connected X11 outputs found.");
            eprintln!("No connected X11 outputs found.");
            std::process::exit(1);
        }
    };

    // --status: print current primary and exit
    if matches.get_flag("status") {
        let (_idx, name) = current_primary_index_name(&outputs);
        // Print only the name, as requested ("simply print")
        if let Some(n) = name {
            println!("Primary monitor: {n}.");
            std::process::exit(0);
        } else {
            // No primary set: print nothing or a marker; here we print "(none)"
            println!("(none)");
            std::process::exit(1); // non-zero to indicate no primary; change to 0 if you prefer
        }
    }

    // Flags
    let auto = matches.get_flag("auto-switch");
    let use_default = matches.get_flag("default");

    if auto {
        // Cycle to the next connected output after current primary
        let (current_idx, current_name) = current_primary_index_name(&outputs);
        let next_idx = ((current_idx.unwrap_or(usize::MAX)).wrapping_add(1)) % outputs.len();
        let target = &outputs[next_idx].name;
        if set_primary(target) {
            notify_ok(&format!(
                "Auto-switched primary: {} -> {}.",
                current_name.unwrap_or("none".into()),
                target
            ));
        } else {
            notify_error(&format!("Failed to set primary to {}.", target));
            std::process::exit(1);
        }
        return;
    }

    if use_default {
        // Try to find preferred monitor from Sway config block
        let cfg_path = matches
            .get_one::<String>("config")
            .map(PathBuf::from)
            .unwrap_or_else(default_sway_config);

        let preferred = read_preferred_from_sway_config(&cfg_path);
        let target_output_name = match preferred {
            Some(hint) => {
                // Map the Sway "nice" identifier to a connector name via swaymsg JSON
                match map_sway_hint_to_connector(&hint) {
                    Some(name) => name,
                    None => {
                        // Maybe the hint is already a connector like "DP-2"
                        hint
                    }
                }
            }
            None => {
                notify_info("No primary monitor set in Sway config. Choosing first monitor.");
                outputs[0].name.clone()
            }
        };

        // Verify target exists in X11 outputs; if not, fall back to first
        let exists = outputs.iter().any(|o| o.name == target_output_name);
        let chosen = if exists {
            target_output_name
        } else {
            outputs[0].name.clone()
        };

        if set_primary(&chosen) {
            notify_ok(&format!("Primary set (default mode) -> {}", chosen));
        } else {
            notify_error(&format!("Failed to set primary to {}", chosen));
            std::process::exit(1);
        }
        return;
    }

    // Interactive mode
    println!("Detected X11 outputs:");
    for (i, o) in outputs.iter().enumerate() {
        println!(
            "  {}. {}{}",
            i + 1,
            o.name,
            if o.primary { "  (current primary)" } else { "" }
        );
    }
    print!("Pick a number to set as primary: ");
    io::stdout().flush().ok();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        notify_error("Failed to read input.");
        std::process::exit(1);
    }
    let idx: usize = match input.trim().parse::<usize>() {
        Ok(n) if (1..=outputs.len()).contains(&n) => n - 1,
        _ => {
            notify_error("Invalid selection.");
            eprintln!("Invalid selection.");
            std::process::exit(1);
        }
    };

    let target = &outputs[idx].name;
    if set_primary(target) {
        notify_ok(&format!("Primary set (interactive) -> {}.", target));
    } else {
        notify_error(&format!("Failed to set primary to {}.", target));
        std::process::exit(1);
    }
}

/* ----------------------- X11/xrandr helpers ----------------------- */

#[derive(Debug, Clone)]
struct XOutput {
    name: String,
    primary: bool,
    connected: bool,
}

fn cmd_ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn xrandr_list_outputs() -> Result<Vec<XOutput>, String> {
    let out = Command::new("xrandr")
        .arg("--query")
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run xrandr: {e}"))?;
    if !out.status.success() {
        return Err("xrandr --query returned non-zero".into());
    }
    let s = String::from_utf8_lossy(&out.stdout);

    // Lines look like:
    // DP-2 connected primary 2560x1440+0+0 ...
    // HDMI-0 connected 1920x1080+2560+0 ...
    // DP-1 disconnected ...
    let re =
        Regex::new(r"^([A-Za-z0-9\-_.+:/]+)\s+(connected|disconnected)(?:\s+primary)?").unwrap();

    let mut outputs = Vec::new();
    for line in s.lines() {
        if let Some(c) = re.captures(line) {
            let name = c.get(1).unwrap().as_str().to_string();
            let status = c.get(2).unwrap().as_str();
            let primary = line.contains(" primary ");
            let connected = status == "connected";
            if connected {
                outputs.push(XOutput {
                    name,
                    primary,
                    connected,
                });
            }
        }
    }

    Ok(outputs)
}

fn current_primary_index_name(outputs: &[XOutput]) -> (Option<usize>, Option<String>) {
    for (i, o) in outputs.iter().enumerate() {
        if o.primary {
            return (Some(i), Some(o.name.clone()));
        }
    }
    (None, None)
}

fn set_primary(output: &str) -> bool {
    Command::new("xrandr")
        .args(["--output", output, "--primary"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/* ----------------------- Sway config helpers ---------------------- */

fn default_sway_config() -> PathBuf {
    let home = env::var("HOME").expect("HOME is not set");
    PathBuf::from(home).join(".config/sway/config")
}

/// Parse the Primary Monitor block:
/// #! Primary Monitor Start !#
/// output "Acer Technologies Acer XF270H B 0x9372943C" resolution ...
/// #! Primary Monitor End !#
/// Returns the string inside quotes after `output`, or None if not present.
fn read_preferred_from_sway_config(path: &PathBuf) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();

    let start_pat = "Primary Monitor Start";
    let end_pat = "Primary Monitor End";
    let re_output_uncommented = regex::Regex::new(r#"^\s*output\s+"([^"]+)""#).ok()?; // note: not matching lines starting with '#'

    // Collect all (start_idx, end_idx) pairs in order
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut search_from = 0;
    while let Some(s) = lines
        .iter()
        .skip(search_from)
        .position(|l| l.contains(start_pat))
    {
        let start_idx = search_from + s;
        let after_start = start_idx + 1;
        if let Some(e) = lines
            .iter()
            .skip(after_start)
            .position(|l| l.contains(end_pat))
        {
            let end_idx = after_start + e; // inclusive block scanning below
            blocks.push((start_idx, end_idx));
            search_from = end_idx + 1;
        } else {
            // unmatched start -> stop scanning further
            break;
        }
    }

    // Examine each block and return the first with an **uncommented** output line
    for (start_idx, end_idx) in blocks {
        for &line in &lines[start_idx..=end_idx] {
            // Skip commented lines outright
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            if let Some(cap) = re_output_uncommented.captures(line) {
                return cap.get(1).map(|m| m.as_str().to_string());
            }
        }
    }

    None
}

/* ----------------- Map Sway "nice" to connector name -------------- */

#[derive(Debug, Deserialize)]
struct SwayOutput {
    name: String,                // e.g., "DP-2"
    make: Option<String>,        // e.g., "Acer Technologies"
    model: Option<String>,       // e.g., "Acer XF270H B"
    serial: Option<String>,      // e.g., "0x9372943C" (or actual serial)
    description: Option<String>, // e.g., "Acer Technologies Acer XF270H B 0x9372943C"
}

#[derive(Debug, Deserialize)]
struct SwayOutputs(Vec<SwayOutput>);

/// Try to map a Sway output hint (either connector like "DP-2" or description like
/// "Acer Technologies Acer XF270H B 0x9372943C") to the connector name.
fn map_sway_hint_to_connector(hint: &str) -> Option<String> {
    // If hint already looks like a connector (DP-#, HDMI-#, eDP-#), just return it.
    if Regex::new(r"^(e?DP|HDMI|DVI|VGA|USB-C|LVDS|Virtual|X11)-")
        .unwrap()
        .is_match(hint)
    {
        return Some(hint.to_string());
    }

    // Else query sway outputs
    let out = Command::new("swaymsg")
        .args(["-t", "get_outputs"])
        .stdout(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json = String::from_utf8_lossy(&out.stdout);
    let vals: serde_json::Value = serde_json::from_str(&json).ok()?;
    let arr = vals.as_array()?;

    for v in arr {
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let desc = v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        // Exact match against description first
        if !desc.is_empty() && desc == hint {
            return Some(name);
        }

        // Fallback: make + model + serial concatenation
        let make = v.get("make").and_then(|x| x.as_str()).unwrap_or("");
        let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("");
        let serial = v.get("serial").and_then(|x| x.as_str()).unwrap_or("");
        let combo = format!("{} {} {}", make, model, serial).trim().to_string();
        if !combo.is_empty() && combo == hint {
            return Some(name);
        }
    }
    None
}

/* --------------------------- Notifications ------------------------ */

fn notify_ok(msg: &str) {
    let _ = Notification::new()
        .summary(APP_SUMMARY)
        .body(msg)
        .icon("video-display")
        .appname(APP_NAME)
        .hint(Hint::Category("Device".to_owned()))
        .timeout(Timeout::Milliseconds(5000))
        .show();
}

fn notify_info(msg: &str) {
    let _ = Notification::new()
        .summary(APP_SUMMARY)
        .body(msg)
        .icon("dialog-information")
        .appname(APP_NAME)
        .timeout(Timeout::Milliseconds(6000))
        .show();
}

fn notify_error(msg: &str) {
    let _ = Notification::new()
        .summary("X11 Primary Switcher — Error")
        .body(msg)
        .icon("dialog-error")
        .appname(APP_NAME)
        .timeout(Timeout::Milliseconds(8000))
        .show();
}
