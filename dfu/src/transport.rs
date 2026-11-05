use std::io::{self, BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crc32fast::Hasher;
use serialport::SerialPort;

use crate::image::{Image, ImageKind, SoftDeviceInfo};

pub(crate) struct Bootloader {
    link: Link,
    max_write: usize,
}

impl Bootloader {
    pub(crate) fn open(name: &str) -> Result<Self> {
        let mut link = Link::open(name)?;
        link.ping()?;
        link.request(Opcode::SetReceiptNotification, &0u16.to_le_bytes())?;

        let response = link.request(Opcode::GetMtu, &[])?;
        let &[low, high, ..] = response.as_slice() else {
            bail!("malformed MTU response {response:02x?}");
        };

        let mtu = usize::from(u16::from_le_bytes([low, high]));
        if mtu < 8 {
            bail!("bootloader MTU of {mtu} bytes is too small");
        }

        Ok(Self {
            link,
            max_write: (mtu - 1) / 2 - 1,
        })
    }

    /// Version and size of the installed SoftDevice
    ///
    /// Returns `None` if there is no SoftDevice, or if the bootloader doesn't support the firmware
    /// version request.
    pub(crate) fn soft_device(&mut self) -> Result<Option<SoftDeviceInfo>> {
        // Image 0 is the bootloader; an installed SoftDevice is always image 1
        let response = match self.link.exchange(Opcode::FirmwareVersion, &[1])? {
            (RESULT_SUCCESS, response) => response,
            (RESULT_INVALID_OPCODE | RESULT_NOT_SUPPORTED, _) => return Ok(None),
            (result, rest) => return Err(failure(Opcode::FirmwareVersion, result, &rest)),
        };

        match response.split_first() {
            Some((&FIRMWARE_TYPE_SOFT_DEVICE, rest)) => Ok(Some(SoftDeviceInfo {
                version: u32_at(rest, 0)?,
                size: u32_at(rest, 8)?,
            })),
            _ => Ok(None),
        }
    }

    pub(crate) fn send(&mut self, image: &Image) -> Result<()> {
        let kind = image.kind();
        self.send_init_packet(&image.encode())
            .with_context(|| format!("bootloader rejected the {kind} init packet"))?;
        self.send_firmware(image.data(), kind)
            .with_context(|| format!("failed to send the {kind}"))
    }

    fn send_init_packet(&mut self, packet: &[u8]) -> Result<()> {
        let max_size = self.max_object_size(ObjectType::Command)?;
        if packet.len() > max_size {
            bail!(
                "init packet is {} bytes, bootloader accepts at most {max_size}",
                packet.len()
            );
        }

        self.create(ObjectType::Command, packet.len())?;
        self.write(packet, &mut Hasher::new(), 0)?;
        self.execute()
    }

    fn send_firmware(&mut self, data: &[u8], kind: ImageKind) -> Result<()> {
        let max_size = self.max_object_size(ObjectType::Data)?;
        let mut hasher = Hasher::new();
        let mut offset = 0;
        for object in data.chunks(max_size) {
            self.create(ObjectType::Data, object.len())?;
            offset = self.write(object, &mut hasher, offset)?;
            self.execute()?;
            eprint!(
                "\rSending {kind} ({} bytes): {:>3}%",
                data.len(),
                offset * 100 / data.len()
            );
        }

        eprintln!();
        Ok(())
    }

    fn max_object_size(&mut self, object: ObjectType) -> Result<usize> {
        let response = self.link.request(Opcode::SelectObject, &[object as u8])?;
        Ok(u32_at(&response, 0)? as usize)
    }

    fn create(&mut self, object: ObjectType, size: usize) -> Result<()> {
        let mut payload = vec![object as u8];
        payload.extend_from_slice(&(size as u32).to_le_bytes());
        self.link.request(Opcode::CreateObject, &payload)?;
        Ok(())
    }

    fn write(&mut self, data: &[u8], hasher: &mut Hasher, offset: usize) -> Result<usize> {
        for chunk in data.chunks(self.max_write) {
            self.link.send(Opcode::WriteObject, chunk)?;
            hasher.update(chunk);
        }

        let offset = offset + data.len();
        let crc = hasher.clone().finalize();
        let response = self.link.request(Opcode::CalculateChecksum, &[])?;
        let (received, received_crc) = (u32_at(&response, 0)? as usize, u32_at(&response, 4)?);
        if (received, received_crc) != (offset, crc) {
            bail!(
                "bootloader received {received} bytes with CRC {received_crc:#010x}, \
                 expected {offset} bytes with CRC {crc:#010x}"
            );
        }

        Ok(offset)
    }

    fn execute(&mut self) -> Result<()> {
        self.link.request(Opcode::Execute, &[])?;
        Ok(())
    }
}

struct Link {
    name: String,
    reader: BufReader<Box<dyn SerialPort>>,
    writer: Box<dyn SerialPort>,
}

impl Link {
    fn open(name: &str) -> Result<Self> {
        let writer = serialport::new(name, BAUD_RATE)
            .timeout(READ_TIMEOUT)
            .dtr_on_open(true)
            .open()
            .with_context(|| format!("failed to open {name}"))?;
        let reader = writer
            .try_clone()
            .with_context(|| format!("failed to clone {name}"))?;

        Ok(Self {
            name: name.to_owned(),
            reader: BufReader::new(reader),
            writer,
        })
    }

    fn ping(&mut self) -> Result<()> {
        let start = Instant::now();
        let mut id = 0u8;
        while start.elapsed() < CONNECT_TIMEOUT {
            id = id.wrapping_add(1);
            self.send(Opcode::Ping, &[id])?;
            let Some(response) = self.receive(PING_TIMEOUT)? else {
                continue;
            };

            match response.as_slice() {
                [RESPONSE, PING, RESULT_SUCCESS, echo] if *echo == id => return Ok(()),
                [RESPONSE, PING, result, ..] if *result != RESULT_SUCCESS => return Ok(()),
                _ => continue,
            }
        }

        bail!("no response from the bootloader on {}", self.name)
    }

    fn request(&mut self, opcode: Opcode, payload: &[u8]) -> Result<Vec<u8>> {
        match self.exchange(opcode, payload)? {
            (RESULT_SUCCESS, rest) => Ok(rest),
            (result, rest) => Err(failure(opcode, result, &rest)),
        }
    }

    /// Send a request and return the result code and payload of its response
    fn exchange(&mut self, opcode: Opcode, payload: &[u8]) -> Result<(u8, Vec<u8>)> {
        self.send(opcode, payload)?;
        let Some(response) = self.receive(RESPONSE_TIMEOUT)? else {
            bail!("no response from the bootloader to {opcode:?}");
        };

        let [RESPONSE, request, result, rest @ ..] = response.as_slice() else {
            bail!("malformed response {response:02x?} to {opcode:?}");
        };

        if *request != opcode as u8 {
            bail!("bootloader answered {request:#04x} instead of {opcode:?}");
        }

        Ok((*result, rest.to_vec()))
    }

    fn send(&mut self, opcode: Opcode, payload: &[u8]) -> Result<()> {
        let mut packet = Vec::with_capacity(2 * (payload.len() + 1) + 1);
        slip_encode(&mut packet, opcode as u8);
        for &byte in payload {
            slip_encode(&mut packet, byte);
        }
        packet.push(SLIP_END);

        self.writer
            .write_all(&packet)
            .with_context(|| format!("failed to write to {}", self.name))
    }

    fn receive(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let start = Instant::now();
        let mut frame = Vec::new();
        loop {
            match self.reader.read_until(SLIP_END, &mut frame) {
                Ok(_) if frame.last() == Some(&SLIP_END) => {
                    frame.pop();
                    if frame.is_empty() {
                        continue;
                    }
                    return slip_decode(&frame).map(Some);
                }
                Ok(_) => bail!("{} was disconnected", self.name),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    if start.elapsed() >= timeout {
                        return Ok(None);
                    }
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to read from {}", self.name));
                }
            }
        }
    }
}

fn slip_encode(packet: &mut Vec<u8>, byte: u8) {
    match byte {
        SLIP_END => packet.extend_from_slice(&[SLIP_ESC, SLIP_ESC_END]),
        SLIP_ESC => packet.extend_from_slice(&[SLIP_ESC, SLIP_ESC_ESC]),
        byte => packet.push(byte),
    }
}

fn slip_decode(frame: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(frame.len());
    let mut bytes = frame.iter();
    while let Some(&byte) = bytes.next() {
        if byte != SLIP_ESC {
            decoded.push(byte);
            continue;
        }

        match bytes.next() {
            Some(&SLIP_ESC_END) => decoded.push(SLIP_END),
            Some(&SLIP_ESC_ESC) => decoded.push(SLIP_ESC),
            other => bail!("invalid SLIP escape sequence {other:02x?}"),
        }
    }

    Ok(decoded)
}

fn u32_at(response: &[u8], index: usize) -> Result<u32> {
    let Some(bytes) = response.get(index..index + 4) else {
        bail!("response {response:02x?} is too short");
    };

    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn failure(opcode: Opcode, result: u8, rest: &[u8]) -> anyhow::Error {
    match result {
        RESULT_EXTENDED_ERROR => anyhow!(
            "{opcode:?} failed: {}",
            extended_error_description(rest.first().copied())
        ),
        code => anyhow!("{opcode:?} failed: {}", result_description(code)),
    }
}

fn result_description(code: u8) -> String {
    let description = match code {
        0x00 => "invalid opcode",
        0x02 => "opcode not supported",
        0x03 => "invalid parameter",
        0x04 => "insufficient resources",
        0x05 => "invalid object",
        0x07 => "unsupported object type",
        0x08 => "operation not permitted",
        0x0a => "operation failed",
        _ => "unknown error",
    };
    format!("{description} ({code:#04x})")
}

fn extended_error_description(code: Option<u8>) -> String {
    let Some(code) = code else {
        return "extended error without code".to_owned();
    };

    let description = match code {
        0x02 => "wrong command format",
        0x03 => "unknown command",
        0x04 => "init packet is invalid",
        0x05 => "firmware version is too low",
        0x06 => "hardware version does not match",
        0x07 => "installed SoftDevice does not match --sd-req",
        0x08 => "signature is missing",
        0x09 => "hash type is not supported",
        0x0a => "hash could not be calculated",
        0x0b => "signature type is not supported",
        0x0c => "firmware hash does not match",
        0x0d => "not enough space on the device",
        _ => "unknown extended error",
    };
    format!("{description} ({code:#04x})")
}

#[derive(Clone, Copy, Debug)]
enum Opcode {
    CreateObject = 0x01,
    SetReceiptNotification = 0x02,
    CalculateChecksum = 0x03,
    Execute = 0x04,
    SelectObject = 0x06,
    GetMtu = 0x07,
    WriteObject = 0x08,
    Ping = 0x09,
    FirmwareVersion = 0x0b,
}

#[derive(Clone, Copy)]
enum ObjectType {
    Command = 0x01,
    Data = 0x02,
}

const BAUD_RATE: u32 = 115_200;
const READ_TIMEOUT: Duration = Duration::from_millis(100);
const PING_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

const PING: u8 = Opcode::Ping as u8;
const RESPONSE: u8 = 0x60;
const RESULT_INVALID_OPCODE: u8 = 0x00;
const RESULT_SUCCESS: u8 = 0x01;
const RESULT_NOT_SUPPORTED: u8 = 0x02;
const RESULT_EXTENDED_ERROR: u8 = 0x0b;

/// Firmware type of a SoftDevice in firmware version responses (unlike in init packets)
const FIRMWARE_TYPE_SOFT_DEVICE: u8 = 0x00;

const SLIP_END: u8 = 0xc0;
const SLIP_ESC: u8 = 0xdb;
const SLIP_ESC_END: u8 = 0xdc;
const SLIP_ESC_ESC: u8 = 0xdd;
