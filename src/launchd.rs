use std::path::Path;

use anyhow::{Context, Result};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// Reproduces the invocation's own flags, minus the subcommand, so the agent runs `run` with identical settings.
pub fn print_plist(label: &str, log_dir: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("resolving executable path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let home = dirs::home_dir().context("cannot determine home directory")?;
    let log_dir = log_dir.map(|p| p.to_path_buf()).unwrap_or_else(|| home.join("Library/Logs/immich-photo-sync"));

    let mut args: Vec<String> = Vec::new();
    let mut raw = std::env::args().skip(1).peekable();
    while let Some(arg) = raw.next() {
        if arg == "launchd-plist" {
            while let Some(next) = raw.peek() {
                if next == "--label" || next == "--log-dir" {
                    raw.next();
                    raw.next();
                } else if next.starts_with("--label=") || next.starts_with("--log-dir=") {
                    raw.next();
                } else {
                    break;
                }
            }
            continue;
        }
        args.push(arg);
    }
    args.push("run".to_string());

    let mut program_args = format!("\t\t<string>{}</string>\n", xml_escape(&exe.to_string_lossy()));
    for a in &args {
        program_args.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(a)));
    }

    println!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{program_args}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>60</integer>
	<key>ProcessType</key>
	<string>Background</string>
	<key>LowPriorityIO</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{out}</string>
	<key>StandardErrorPath</key>
	<string>{err}</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
</dict>
</plist>"#,
        label = xml_escape(label),
        out = xml_escape(&log_dir.join("immich-photo-sync.log").to_string_lossy()),
        err = xml_escape(&log_dir.join("immich-photo-sync.err.log").to_string_lossy()),
    );
    eprintln!();
    eprintln!("# Install with:");
    eprintln!("#   mkdir -p {}", log_dir.display());
    eprintln!("#   <this command> > ~/Library/LaunchAgents/{label}.plist");
    eprintln!("#   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{label}.plist");
    Ok(())
}
