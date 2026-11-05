use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};

mod image;
use image::{Application, Fwid, MBR_END, SoftDevice};
mod transport;
use transport::Bootloader;

fn main() -> Result<()> {
    let Args { command } = Args::parse();
    match command {
        Command::Flash(cmd) => cmd.run(),
        Command::List => {
            let ports = ports()?;
            if ports.is_empty() {
                eprintln!(
                    "No dongles in bootloader mode found; press a dongle's reset button to start its \
             bootloader"
                );
                return Ok(());
            }

            for (name, usb) in ports {
                match usb.serial_number {
                    Some(serial) => println!("{name} (serial number {serial})"),
                    None => println!("{name}"),
                }
            }

            Ok(())
        }
    }
}

/// Flash an nRF52840 Dongle through Nordic's USB DFU bootloader
///
/// To flash a dongle:
///
///   1. Plug it in and press its sideways-facing RESET button to start the bootloader. The red
///      LED pulses while the bootloader is running.
///   2. Run `dfu list` to check that the dongle shows up.
///   3. Run `dfu flash <ELF>` to flash the application.
///
/// An application linked at 0x1000 (directly after the MBR) runs without a SoftDevice. An
/// application linked after SoftDevice S140 7.3.0 (at 0x27000) needs that SoftDevice. Pass it
/// along with the application; it is only sent if the dongle doesn't have it yet:
///
///   dfu flash --soft-device s140_nrf52_7.3.0_softdevice.hex <ELF>
///
/// Once it's installed, you can also pass its firmware ID instead:
///
///   dfu flash --sd-req 0x0123 <ELF>
///
/// To flash on `cargo run`, use `dfu flash` as the cargo runner (see thread/.cargo/config.toml).
/// Arguments after `--` are passed on to it, as in `cargo run -- --sd-req 0x0123`.
#[derive(Parser)]
#[command(verbatim_doc_comment)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Flash an application, and optionally a SoftDevice, to a dongle in bootloader mode
    ///
    /// The application is placed after the MBR (at 0x1000) or, when a SoftDevice is installed,
    /// directly after the SoftDevice.
    Flash(Flash),
    /// List connected dongles that are in bootloader mode
    List,
}

#[derive(clap::Args)]
struct Flash {
    /// Application ELF file to flash
    elf: PathBuf,
    /// SoftDevice HEX file to install before the application
    ///
    /// For example `s140_nrf52_7.3.0_softdevice.hex` as distributed by Nordic. Skipped if the
    /// bootloader reports an installed SoftDevice with the same version and size.
    #[arg(long)]
    soft_device: Option<PathBuf>,
    /// Firmware ID of the SoftDevice currently installed on the device
    ///
    /// Use 0x00 if there is none. Defaults to 0x00 when installing a SoftDevice or flashing an
    /// application linked at 0x1000.
    #[arg(long, value_parser = Fwid::parse)]
    sd_req: Option<Fwid>,
    /// Serial port of the bootloader (see `dfu list`), detected by its USB ID if omitted
    #[arg(long)]
    port: Option<String>,
}

impl Flash {
    fn run(self) -> Result<()> {
        let Flash {
            elf,
            soft_device,
            sd_req,
            port,
        } = self;

        let application = Application::from_elf(&elf)?;
        let (soft_device, app_sd_req) = match soft_device {
            Some(path) => {
                let soft_device = SoftDevice::from_hex(&path)?;
                if application.start() != soft_device.end {
                    bail!(
                        "application starts at {:#x}, but the SoftDevice in {} ends at {:#x}",
                        application.start(),
                        path.display(),
                        soft_device.end,
                    );
                }

                let fwid = soft_device.fwid;
                (Some(soft_device), fwid)
            }
            None => {
                let start = application.start();
                let sd_req = match (sd_req, start == MBR_END) {
                    (None | Some(Fwid::NONE), true) => Fwid::NONE,
                    (Some(Fwid::NONE), false) => bail!(
                        "--sd-req 0x00 means there is no SoftDevice, but the application starts at {start:#x}"
                    ),
                    (Some(fwid), false) => fwid,
                    (None, false) => bail!(
                        "application starts at {start:#x}, so it needs a SoftDevice; pass --sd-req with the \
                         firmware ID of the installed SoftDevice (0x0123 for S140 7.3.0), or --soft-device to \
                         install one"
                    ),
                    (Some(fwid), true) => bail!(
                        "application starts at {MBR_END:#x}, so it can't run alongside SoftDevice {fwid}; \
                         link it after the SoftDevice or drop --sd-req"
                    ),
                };

                (None, sd_req)
            }
        };

        let dongle = match &port {
            Some(port) => Dongle::Port(port),
            None => Dongle::Any,
        };
        let mut name = dongle.wait_for_bootloader(
            "Waiting for the bootloader; press the dongle's reset button to start it",
            None,
        )?;

        let serial_number = ports()?
            .into_iter()
            .find(|(port, _)| port == &name)
            .and_then(|(_, usb)| usb.serial_number);

        let mut bootloader = Bootloader::open(&name)?;
        if let Some(soft_device) = soft_device {
            if bootloader.soft_device()? == Some(soft_device.info) {
                eprintln!("SoftDevice {} is already installed", soft_device.info);
            } else {
                bootloader.send(&soft_device.into_image(sd_req.unwrap_or(Fwid::NONE)))?;
                drop(bootloader);
                name = wait_for_restart(&name, serial_number.as_deref())?;
                bootloader = Bootloader::open(&name)?;
            }
        }

        bootloader.send(&application.into_image(app_sd_req))?;
        eprintln!("Done");
        Ok(())
    }
}

/// Wait for the bootloader on `name` to restart after an update, returning its new port name
fn wait_for_restart(name: &str, serial_number: Option<&str>) -> Result<String> {
    let start = Instant::now();
    while Path::new(name).exists() {
        if start.elapsed() > RESTART_TIMEOUT {
            bail!("bootloader on {name} did not restart after the update");
        }

        thread::sleep(POLL_INTERVAL);
    }

    let dongle = match serial_number {
        Some(serial_number) => Dongle::SerialNumber(serial_number),
        None => Dongle::Port(name),
    };
    dongle.wait_for_bootloader(
        "Waiting for the bootloader to restart",
        Some(RESTART_TIMEOUT),
    )
}

/// How to recognize the dongle to flash
#[derive(Clone, Copy)]
enum Dongle<'a> {
    /// Any dongle running the bootloader
    Any,
    /// The dongle on the given serial port
    Port(&'a str),
    /// The dongle running the bootloader with the given USB serial number
    SerialNumber(&'a str),
}

impl Dongle<'_> {
    fn wait_for_bootloader(self, hint: &str, timeout: Option<Duration>) -> Result<String> {
        let start = Instant::now();
        let mut hinted = false;
        loop {
            let found = match self {
                Dongle::Any => ports()?.into_iter().next().map(|(name, _)| name),
                Dongle::Port(name) => Path::new(name).exists().then(|| name.to_owned()),
                Dongle::SerialNumber(serial_number) => {
                    ports()?.into_iter().find_map(|(name, usb)| {
                        match usb.serial_number.as_deref() == Some(serial_number) {
                            true => Some(name),
                            false => None,
                        }
                    })
                }
            };

            if let Some(name) = found {
                return Ok(name);
            }

            if let Some(timeout) = timeout
                && start.elapsed() > timeout
            {
                bail!("bootloader did not come back within {timeout:?}");
            }

            if !hinted {
                eprintln!("{hint}");
                hinted = true;
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Serial ports of connected dongles running the bootloader
fn ports() -> Result<Vec<(String, UsbPortInfo)>> {
    let ports = serialport::available_ports().context("failed to list serial ports")?;
    let mut bootloaders = Vec::new();
    for SerialPortInfo {
        port_name,
        port_type,
    } in ports
    {
        let SerialPortType::UsbPort(usb) = port_type else {
            continue;
        };

        if usb.vid != NORDIC_VID || usb.pid != OPEN_BOOTLOADER_PID {
            continue;
        }

        if port_name.starts_with("/dev/tty.") {
            continue;
        }

        bootloaders.push((port_name, usb));
    }

    Ok(bootloaders)
}

const NORDIC_VID: u16 = 0x1915;
const OPEN_BOOTLOADER_PID: u16 = 0x521f;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);
