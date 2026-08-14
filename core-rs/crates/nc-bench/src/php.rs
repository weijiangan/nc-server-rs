//! php-fpm measurement-state enforcement (Phase 17 addition, 2026-08-14).
//!
//! The dev image ships xdebug, and a stack brought up without
//! `PHP_XDEBUG_MODE=off` runs it in `develop` mode: develop instruments every
//! function call, ~2.1-2.6× on PHP request handling (measured on this stack:
//! status.php 4.9 ms → 13.5 ms).  A drifted stack silently taxes every
//! PHP-side number and inflates the benchmark ratios — the same class of
//! measurement hazard as the bruteforce throttle (`auth::reset_throttle`).
//!
//! `ensure_xdebug_off` forces `xdebug.mode=off` on an instance's php-fpm
//! **without touching any persistent config**: it writes a
//! `zz-bench-xdebug.ini` override into the container's ephemeral
//! `/usr/local/etc/php/conf.d` layer (loaded after `xdebug.ini`, so it wins)
//! and gracefully reloads php-fpm (`USR2`, new workers re-read the ini).
//! The container's writable layer is recreated from the image on the next
//! `make sut-image`/`up`, so the override never survives a bring-up — a
//! drifted stack is re-enforced on every bench run.  Failure is warned, not
//! fatal (same style as `reset_throttle`): the numbers then include the
//! xdebug tax.

use std::time::Duration;

use nc_difftest::config::Instance;

/// Run `args` inside the instance's container, returning trimmed stdout.
fn docker_exec(inst: &Instance, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("docker")
        .args(["exec", &inst.container])
        .args(args)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(format!(
            "docker exec exited {:?}: {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// The effective xdebug mode the php-fpm workers run with (`None` = could
/// not inspect).  CLI and FPM read the same conf.d, and `xdebug.mode` is a
/// system ini read at process start, so `php -i`'s value is the workers'
/// value.  This xdebug build reports `mode=off` as an empty string.
fn detect_mode(inst: &Instance) -> Option<String> {
    docker_exec(inst, &["php", "-r", "echo ini_get('xdebug.mode');"]).ok()
}

/// Force `xdebug.mode=off` on the instance's php-fpm before measuring.
pub fn ensure_xdebug_off(inst: &Instance) {
    let mode = match detect_mode(inst) {
        Some(m) => m,
        None => {
            eprintln!(
                "  warn: cannot inspect xdebug.mode on {} — numbers may include the xdebug tax",
                inst.base_url
            );
            return;
        }
    };
    if mode.is_empty() || mode == "off" {
        eprintln!("  php:  xdebug.mode={mode:?} on {} — clean", inst.base_url);
        return;
    }
    eprintln!(
        "  php:  xdebug.mode={mode:?} on {} — forcing off (measurement hazard)",
        inst.base_url
    );
    if let Err(e) = docker_exec(
        inst,
        &[
            "sh",
            "-c",
            "printf 'xdebug.mode=off\\n' > /usr/local/etc/php/conf.d/zz-bench-xdebug.ini && pkill -USR2 -x php-fpm",
        ],
    ) {
        eprintln!(
            "  warn: failed to force xdebug.mode=off on {} ({e}) — numbers may include the xdebug tax",
            inst.base_url
        );
        return;
    }
    // USR2 is a graceful reload; give the new workers a moment to respawn
    // (they re-read the ini), then verify the override took.
    std::thread::sleep(Duration::from_millis(1200));
    match detect_mode(inst) {
        Some(m) if m.is_empty() || m == "off" => {
            eprintln!("  php:  verified xdebug.mode=off on {}", inst.base_url);
        }
        Some(m) => {
            eprintln!(
                "  warn: xdebug.mode still {m:?} on {} — numbers may include the xdebug tax",
                inst.base_url
            );
        }
        None => {}
    }
}
