use std::io::Write;

use flate2::{
    write::{ZlibDecoder, ZlibEncoder},
    Compression,
};

use super::FlashTarget;
use crate::{
    command::{Command, CommandType},
    connection::{Connection, USB_SERIAL_JTAG_PID},
    elf::RomSegment,
    error::Error,
    flasher::{SpiAttachParams, FLASH_SECTOR_SIZE},
    targets::Chip,
};

/// Applications running from an ESP32's (or variant's) flash
pub struct Esp32Target {
    chip: Chip,
    spi_attach_params: SpiAttachParams,
    use_stub: bool,
}

impl Esp32Target {
    pub fn new(chip: Chip, spi_attach_params: SpiAttachParams, use_stub: bool) -> Self {
        Esp32Target {
            chip,
            spi_attach_params,
            use_stub,
        }
    }
}

impl FlashTarget for Esp32Target {
    fn begin(&mut self, connection: &mut Connection) -> Result<(), Error> {
        connection.with_timeout(CommandType::SpiAttach.timeout(), |connection| {
            let command = if self.use_stub {
                Command::SpiAttachStub {
                    spi_params: self.spi_attach_params,
                }
            } else {
                Command::SpiAttach {
                    spi_params: self.spi_attach_params,
                }
            };

            connection.command(command)
        })?;

        // TODO: Remove this when we use the stub, the stub should be taking care of
        //       this.
        // TODO: Do we also need to disable RTC super WDT?
        if connection.get_usb_pid()? == USB_SERIAL_JTAG_PID {
            match self.chip {
                Chip::Esp32c3 => {
                    connection.command(Command::WriteReg {
                        address: 0x6000_80a8,
                        value: 0x50D8_3AA1,
                        mask: None,
                    })?; // WP disable
                    connection.command(Command::WriteReg {
                        address: 0x6000_8090,
                        value: 0x0,
                        mask: None,
                    })?; // turn off RTC WDT
                    connection.command(Command::WriteReg {
                        address: 0x6000_80a8,
                        value: 0x0,
                        mask: None,
                    })?; // WP enable
                }
                Chip::Esp32s3 => {
                    connection.command(Command::WriteReg {
                        address: 0x6000_80B0,
                        value: 0x50D8_3AA1,
                        mask: None,
                    })?; // WP disable
                    connection.command(Command::WriteReg {
                        address: 0x6000_8098,
                        value: 0x0,
                        mask: None,
                    })?; // turn off RTC WDT
                    connection.command(Command::WriteReg {
                        address: 0x6000_80B0,
                        value: 0x0,
                        mask: None,
                    })?; // WP enable
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn write_segment(
        &mut self,
        connection: &mut Connection,
        segment: RomSegment,
        progress_cb: Option<Box<dyn Fn(usize, usize)>>,
    ) -> Result<(), Error> {
        let addr = segment.addr;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&segment.data)?;
        let compressed = encoder.finish()?;

        let target = self.chip.into_target();
        let flash_write_size = target.flash_write_size(connection)?;
        let block_count = (compressed.len() + flash_write_size - 1) / flash_write_size;
        let erase_count = (segment.data.len() + FLASH_SECTOR_SIZE - 1) / FLASH_SECTOR_SIZE;

        // round up to sector size
        let erase_size = (erase_count * FLASH_SECTOR_SIZE) as u32;

        connection.with_timeout(
            CommandType::FlashDeflateBegin.timeout_for_size(erase_size),
            |connection| {
                connection.command(Command::FlashDeflateBegin {
                    size: segment.data.len() as u32,
                    blocks: block_count as u32,
                    block_size: flash_write_size as u32,
                    offset: addr,
                    supports_encryption: self.chip != Chip::Esp32 && !self.use_stub,
                })?;
                Ok(())
            },
        )?;

        let chunks = compressed.chunks(flash_write_size);
        let num_chunks = chunks.len();

        // decode the chunks to see how much data the device will have to save
        let mut decoder = ZlibDecoder::new(Vec::new());
        let mut decoded_size = 0;

        for (i, block) in chunks.enumerate() {
            decoder.write_all(block)?;
            decoder.flush()?;
            let size = decoder.get_ref().len() - decoded_size;
            decoded_size = decoder.get_ref().len();

            connection.with_timeout(
                CommandType::FlashDeflateData.timeout_for_size(size as u32),
                |connection| {
                    connection.command(Command::FlashDeflateData {
                        sequence: i as u32,
                        pad_to: 0,
                        pad_byte: 0xff,
                        data: block,
                    })?;
                    Ok(())
                },
            )?;

            if let Some(ref cb) = progress_cb {
                cb(i + 1, num_chunks);
            }
        }

        Ok(())
    }

    fn finish(&mut self, connection: &mut Connection, reboot: bool) -> Result<(), Error> {
        connection.with_timeout(CommandType::FlashDeflateEnd.timeout(), |connection| {
            connection.command(Command::FlashDeflateEnd { reboot: false })
        })?;

        if reboot {
            connection.reset()?;
        }

        Ok(())
    }
}