use std::{error::Error, fs, path::Path};

use crate::{AppConfig, CarPlayOutputManager, HomeKitManager, MfiManager};
// use catplay_iap2_usb_gadget::gadget::GadgetHelper;
use catplay_util::{EventReconciler, EventSleeper};
use log::{info, warn};
use tokio::signal;

fn parse_config(path: String) -> Result<AppConfig, String> {
    let config_str: String = fs::read_to_string(path).map_err(|err| format!("Failed to read config file: {}", err))?;
    let config: AppConfig = toml::from_str(&config_str).map_err(|err| format!("Failed to parse config file: {}", err))?;
    Ok(config)
}

pub struct Main {
    // usb: Option<UsbInputManager>,
    // usb_out: Option<UsbOutputManager>,
    // bt: Option<BluetoothManager>,
    // wifi: Option<WifiManager>,
    mfi: Option<MfiManager>,
    // airplay: Option<AirPlayManager>,
    homekit: Option<HomeKitManager>,
    // input: Option<CarPlayInputManager>,
    output: Option<CarPlayOutputManager>,
}

async fn wait_forever() -> Result<(), Box<dyn Error>> {
    signal::ctrl_c().await.expect("failed to listen for event");
    unsafe {
        libc::close(0);
    }

    Ok(())
}

impl Main {
    pub async fn start() -> Result<Main, Box<dyn Error>> {
        let config = parse_config("./c2a.toml".into()).or(parse_config("/etc/catplay/catplay.conf".into()))?;

        if let Some(dir) = config.persist_dir.as_deref() {
            match crate::boot_journal::start(Path::new(dir)) {
                Ok(seq) => info!("Boot journal: this is boot {seq} ({dir}/journal)"),
                Err(err) => warn!("Boot journal disabled: {err}"),
            }
        }

        // if let Err(_err) = GadgetHelper::cleanup_once() {
        //     error!("Failed to clean up gadgets, continuing anyway");
        // }

        // GadgetHelper::cleanup_once()?;

        let mut mfi = MfiManager::new();
        mfi.start(&config)?;

        let mut homekit = HomeKitManager::default();
        homekit.start(&config, &mfi)?;

        let mut output = CarPlayOutputManager::new();
        output.start(&config, &mfi, &homekit)?;

        Ok(Main {
            mfi: Some(mfi),
            homekit: Some(homekit),
            // input: Some(input),
            output: Some(output),
        })
    }

    pub async fn do_loop(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            let output = self.output.as_mut().unwrap();
            let _ = output.reconcile().await;
            output.sleep().await;
        }

        #[allow(unreachable_code)]
        wait_forever().await?;
    }
}
