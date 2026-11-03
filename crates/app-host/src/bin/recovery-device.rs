//! Local, explicit recovery-device enrollment for an authenticated desktop account.
//! No signing interface is exposed to the webview or network.
#[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
fn run() -> Result<(), String> {
    use isyncyou_core::config::Config;
    use isyncyou_engine::self_hosted_mail_restore::{
        initialize_linux_recovery_signer, load_linux_recovery_signer,
        prove_linux_recovery_device, LinuxRegistrationOutcome,
    };
    let args: Vec<String> = std::env::args().collect();
    if !(args.len() == 4 && args[1] == "init" || args.len() == 5 && args[1] == "prove") {
        return Err("usage: recovery-device init CONFIG ACCOUNT | prove CONFIG ACCOUNT OPERATION".into());
    }
    let path = std::path::Path::new(&args[2]);
    let account = &args[3];
    Config::with_config_lock(path, || {
        let cfg = Config::load(path)?;
        cfg.validate().map_err(|_| "account_config_unavailable")?;
        match args[1].as_str() {
            "init" => {
                initialize_linux_recovery_signer(&cfg, account)?;
                println!("{{\"state\":\"initialized\"}}");
            }
            "prove" => {
                let signer = load_linux_recovery_signer(&cfg, account)?;
                match prove_linux_recovery_device(&cfg, account, &signer, &args[4])? {
                    LinuxRegistrationOutcome::Applied(value) | LinuxRegistrationOutcome::AlreadyCompleted(value) => {
                        let object = value.as_object().ok_or("invalid_registration_result")?;
                        let fields = ["challenge_id", "account", "device", "fingerprint", "state"];
                        if object.len() != fields.len() || fields.iter().any(|key| object.get(*key).and_then(serde_json::Value::as_str).is_none()) || object.get("state").and_then(serde_json::Value::as_str) != Some("awaiting_management") {
                            return Err("invalid_registration_result".into());
                        }
                        println!("{}", value);
                    }
                    LinuxRegistrationOutcome::Unknown => println!("{{\"state\":\"outcome_unknown\"}}"),
                }
            }
            _ => return Err("invalid_operation".into()),
        }
        Ok(())
    })
}

fn main() {
    #[cfg(all(target_os = "linux", feature = "desktop-keyring"))]
    if let Err(error) = run() {
        eprintln!("recovery device enrollment failed: {error}");
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", feature = "desktop-keyring")))]
    {
        eprintln!("recovery device enrollment requires Linux desktop keyring support");
        std::process::exit(1);
    }
}
