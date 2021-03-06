use stm32l4xx_hal::stm32;

#[cfg(feature = "stm32l4x1")]
type SDMMC = stm32::SDMMC;
#[cfg(feature = "stm32l4x6")]
type SDMMC = stm32::SDMMC1;

use crate::Error::*;
use crate::{
    AppCommand, Block, BlockCount, BlockIndex, BusWidth, CardHost, CardVersion, Command, Error,
    BLOCK_SIZE, CSD,
};
use nb::block;
use nb::Error::{Other, WouldBlock};

const SDMMC1_ADDRESS: u32 = 0x4001_2800;
const FIFO_OFFSET: u32 = 0x80;
const SEND_IF_COND_PATTERN: u32 = 0x0000_01aa;
const STATUS_ERROR_MASK: u32 = 0x0000_05ff;

use stm32l4xx_hal::gpio;
type Pin = gpio::Alternate<gpio::AF12, gpio::Input<gpio::Floating>>;
type ClockPin = gpio::gpioc::PC12<Pin>;
type CommandPin = gpio::gpiod::PD2<Pin>;
type Data0Pin = gpio::gpioc::PC8<Pin>;
type Data1Pin = gpio::gpioc::PC9<Pin>;
type Data2Pin = gpio::gpioc::PC10<Pin>;
type Data3Pin = gpio::gpioc::PC11<Pin>;
pub type Pins = (ClockPin, CommandPin, Data0Pin, Data1Pin, Data2Pin, Data3Pin);

#[derive(Copy, Clone, Debug)]
enum State {
    Uninitialized,
    Init1(bool),
    Ready,
    Reading,
    Writing,
}

pub struct Device {
    sdmmc: SDMMC,
    dma: stm32::DMA2,
    pins: Pins,
    config: Config,
    state: State,
    rca: u32,
    /// Card Specific Data
    csd: CSD,
    card_version: CardVersion,
}

pub struct Config {
    /// The width of the data bus in bits, either one or four.
    pub bus_width: BusWidth,
    /// Value to divide the clock speed by. Zero or one means bypass clock divider.
    pub clock_divider: u8,
    /// The number of clock cycles to wait for data transfer to complete.
    pub data_timeout: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bus_width: BusWidth::Bits1,
            clock_divider: 4,
            data_timeout: 0x1000000,
        }
    }
}

impl Device {
    pub fn new(sdmmc: SDMMC, dma: stm32::DMA2, pins: Pins, config: Config) -> Device {
        Device {
            sdmmc,
            dma,
            pins,
            config,
            state: State::Uninitialized,
            rca: 0,
            csd: CSD::V1([0; 4]),
            card_version: CardVersion::V1SC,
        }
    }

    /// Recycle the object to get back the SDMMC and DMA peripherals. Panics if an operation is
    /// still ongoing.
    pub fn free(self) -> (SDMMC, stm32::DMA2, Pins) {
        use State::*;
        match self.state {
            Uninitialized | Init1(_) | Ready => (self.sdmmc, self.dma, self.pins),
            Reading | Writing => panic!("attempt to free card host while still in use"),
        }
    }

    fn init_peri(&mut self, clock_divider: u8) {
        // Enable power, then clock.
        self.sdmmc
            .clkcr
            .modify(|_, w| w.negedge().set_bit().pwrsav().set_bit().clken().clear_bit());

        if clock_divider < 2 {
            self.sdmmc.clkcr.modify(|_, w| w.bypass().set_bit());
        } else {
            self.sdmmc
                .clkcr
                .modify(|_, w| unsafe { w.clkdiv().bits(clock_divider - 2) });
        }

        self.sdmmc.clkcr.modify(|_, w| unsafe {
            w.widbus().bits(match self.config.bus_width {
                BusWidth::Bits1 => 0,
                BusWidth::Bits4 => 1,
            })
        });

        self.sdmmc
            .power
            .modify(|_, w| unsafe { w.pwrctrl().bits(3) });
        self.sdmmc.clkcr.modify(|_, w| w.clken().set_bit());

        // Set the data timeout.
        self.sdmmc
            .dtimer
            .write(|w| unsafe { w.bits(self.config.data_timeout) });

        // Select sdmmc for dma 2 channel 4.
        self.dma.cselr.modify(|_, w| w.c4s().bits(0x7));
    }

    pub fn status(&self) -> u32 {
        self.sdmmc.sta.read().bits()
    }

    fn check_operating_conditions(&mut self) -> Result<(), Error> {
        match self.card_command_short(Command::SEND_IF_COND, SEND_IF_COND_PATTERN) {
            Err(e) => Err(e),
            Ok(received_pattern) => {
                if received_pattern != SEND_IF_COND_PATTERN {
                    Err(OperatingConditionsNotSupported)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn app_command_short(&mut self, cmd: AppCommand, arg: u32) -> Result<u32, Error> {
        self.card_command_short(Command::APP_COMMAND, self.rca)?;
        self.sdmmc.arg.write(|w| unsafe { w.bits(arg) });
        self.sdmmc.cmd.write(|w| unsafe {
            w.cmdindex()
                .bits(cmd as u8)
                .waitresp()
                .bits(1)
                .cpsmen()
                .set_bit()
        });

        block!(self.check_command(true))?;
        Ok(self.sdmmc.resp1.read().bits())
    }

    fn acmd41(&mut self, hcs: bool) -> Result<u32, Error> {
        self.card_command_short(Command::APP_COMMAND, 0)?;
        let arg = 0x0010_0000 | (hcs as u32) << 30;
        self.sdmmc.arg.write(|w| unsafe { w.bits(arg) });
        self.sdmmc.cmd.write(|w| unsafe {
            w.cmdindex()
                .bits(AppCommand::SD_SEND_OP_COND as u8)
                .waitresp()
                .bits(1)
                .cpsmen()
                .set_bit()
        });

        // acmd41 does not set crc so we expect crcfail
        match block!(self.check_command(true)) {
            Err(CRCFail) => Ok(()),
            x => x,
        }?;

        Ok(self.sdmmc.resp1.read().bits())
    }

    fn card_command_none(&mut self, cmd: Command, arg: u32) -> Result<(), Error> {
        self.sdmmc.arg.write(|w| unsafe { w.bits(arg) });
        self.sdmmc.cmd.write(|w| unsafe {
            w.cmdindex()
                .bits(cmd as u8)
                .waitresp()
                .bits(0)
                .cpsmen()
                .set_bit()
        });

        block!(self.check_command(false))
    }

    fn card_command_short(&mut self, cmd: Command, arg: u32) -> Result<u32, Error> {
        self.sdmmc.arg.write(|w| unsafe { w.bits(arg) });
        self.sdmmc.cmd.write(|w| unsafe {
            w.cmdindex()
                .bits(cmd as u8)
                .waitresp()
                .bits(1)
                .cpsmen()
                .set_bit()
        });

        block!(self.check_command(true))?;
        if self.sdmmc.respcmd.read().respcmd().bits() != cmd as u8 {
            return Err(UnexpectedResponse);
        }

        Ok(self.sdmmc.resp1.read().bits())
    }

    fn card_command_long(&mut self, cmd: Command, arg: u32) -> Result<[u32; 4], Error> {
        self.sdmmc.arg.write(|w| unsafe { w.bits(arg) });
        self.sdmmc.cmd.write(|w| unsafe {
            w.cmdindex()
                .bits(cmd as u8)
                .waitresp()
                .bits(3)
                .cpsmen()
                .set_bit()
        });

        block!(self.check_command(true))?;
        // This delay helps with command recognition in the logic analyzer.
        // TODO: Remove
        let foo = 0u32;
        for _ in 0..0x1000 {
            unsafe {
                core::ptr::read_volatile(&foo);
            }
        }

        Ok([
            self.sdmmc.resp1.read().bits(),
            self.sdmmc.resp2.read().bits(),
            self.sdmmc.resp3.read().bits(),
            self.sdmmc.resp4.read().bits(),
        ])
    }

    fn check_ready(&mut self) -> Result<(), Error> {
        use State::*;
        match self.state {
            Uninitialized | Init1(_) => Err(Error::Uninitialized),
            Ready => {
                self.init_peri(self.config.clock_divider);
                Ok(())
            }
            Reading | Writing => Err(Error::Busy),
        }
    }

    fn check_command(&mut self, expect_response: bool) -> nb::Result<(), Error> {
        let status = self.sdmmc.sta.read();
        if status.cmdact().bit() {
            return Err(WouldBlock);
        }
        self.sdmmc
            .icr
            .write(|w| unsafe { w.bits(STATUS_ERROR_MASK) });
        if status.ccrcfail().bit() {
            Err(Other(CRCFail))
        } else if status.ctimeout().bit() {
            Err(Other(Timeout))
        } else if expect_response && !status.cmdrend().bit() {
            Err(Other(UnknownResult))
        } else if !expect_response && !status.cmdsent().bit() {
            Err(Other(UnknownResult))
        } else {
            Ok(())
        }
    }
}

impl CardHost for Device {
    fn init_card(&mut self) -> nb::Result<(), Error> {
        use State::*;
        match self.state {
            Reading | Writing => panic!("attempt to reinit card while still in use"),

            Uninitialized | Ready => {
                self.init_peri(0x80);
                // * -> idle
                self.card_command_none(Command::GO_IDLE_STATE, 0)?;
                // Determine card version.
                let v2 = match self.check_operating_conditions() {
                    Err(Timeout) => false,
                    Ok(_) => true,
                    Err(e) => return Err(Other(e)),
                };

                self.state = Init1(v2);
                // Recurse once to start the next part.
                self.init_card()
            }

            Init1(v2) => {
                self.init_peri(0x80);
                // idle -> ready
                let result = match self.acmd41(v2) {
                    Err(Timeout) if !v2 => Err(Other(NoCard)),
                    Ok(result) if result >> 31 == 0 => Err(WouldBlock),
                    Err(e) => Err(Other(e)),
                    Ok(x) => Ok(x),
                }?;

                self.state = Uninitialized;
                let ccs = (result >> 30) & 1 != 0;
                self.card_version = match (v2, ccs) {
                    (false, _) => CardVersion::V1SC,
                    (true, false) => CardVersion::V2SC,
                    (true, true) => CardVersion::V2HC,
                };

                // ready -> ident
                self.card_command_long(Command::ALL_SEND_CID, 0)?;

                // ident -> stby
                let card_rca_status = self.card_command_short(Command::SEND_RELATIVE_ADDR, 0)?;
                self.rca = card_rca_status & 0xffff_0000;
                let csd = self.card_command_long(Command::SEND_CSD, self.rca)?;
                self.csd = match self.card_version {
                    CardVersion::V1SC | CardVersion::V2SC => CSD::V1(csd),
                    CardVersion::V2HC => CSD::V2(csd),
                };

                // stby -> tran
                self.card_command_short(Command::SELECT_CARD, self.rca)?;
                self.app_command_short(
                    AppCommand::SET_BUS_WIDTH,
                    match self.config.bus_width {
                        BusWidth::Bits1 => 0,
                        BusWidth::Bits4 => 2,
                    },
                )?;

                self.state = Ready;
                Ok(())
            }
        }
    }

    fn card_size(&mut self) -> Result<BlockCount, Error> {
        match self.state {
            State::Uninitialized => Err(Error::Uninitialized),
            State::Init1(_) => Err(Error::Uninitialized),
            _ => Ok(self.csd.capacity()),
        }
    }

    #[allow(unused_unsafe)]
    unsafe fn read_block(&mut self, block: &mut Block, address: BlockIndex) -> Result<(), Error> {
        self.check_ready()?;

        // a. Set the data length register.
        self.sdmmc
            .dlen
            .write(|w| unsafe { w.bits(block.len() as u32) });

        // b. Set the dma channel.
        //    - Clear any pending interrupts.
        self.dma.ifcr.write(|w| w.cgif4().set_bit());
        //    - Set the channel source address.
        self.dma
            .cmar4
            .write(|w| unsafe { w.bits(block as *const Block as u32) });
        //    - Set the channel destination address.
        self.dma
            .cpar4
            .write(|w| unsafe { w.bits(SDMMC1_ADDRESS + FIFO_OFFSET) });
        //    - Set the number of words to transfer.
        self.dma.cndtr4.write(|w| w.ndt().bits(0x80));

        //    - Set the word size, direction and increments.
        self.dma.ccr4.write(|w| {
            w.dir()
                .clear_bit()
                .minc()
                .set_bit()
                .pinc()
                .clear_bit()
                .msize()
                .bits32()
                .psize()
                .bits32()
        });

        //    - Enable the channel.
        self.dma.ccr4.modify(|_, w| w.en().set_bit());
        // c. Set the data control register:
        self.sdmmc.dctrl.write(|w| unsafe {
            w.dten()
                .set_bit()
                .dtdir()
                .set_bit()
                .dmaen()
                .set_bit()
                .dblocksize()
                .bits(0x9)
        });

        // d. Set the address.
        // e. Set the command register.
        self.card_command_short(Command::READ_BLOCK, address.0)?;
        self.state = State::Reading;
        Ok(())
    }

    #[allow(unused_unsafe)]
    unsafe fn write_blocks(&mut self, blocks: &[Block], address: BlockIndex) -> Result<(), Error> {
        self.check_ready()?;

        self.card_command_short(Command::SET_BLOCK_COUNT, blocks.len() as u32)?;

        // a. Set the data length register.
        self.sdmmc
            .dlen
            .write(|w| unsafe { w.bits((blocks.len() * BLOCK_SIZE) as u32) });

        // b. Set the dma channel.
        //    - Set the channel source address.
        self.dma
            .cmar4
            .write(|w| unsafe { w.bits(blocks as *const [Block] as *const Block as u32) });
        //    - Set the channel destination address.
        self.dma
            .cpar4
            .write(|w| unsafe { w.bits(SDMMC1_ADDRESS + FIFO_OFFSET) });
        //    - Set the number of words to transfer.
        self.dma
            .cndtr4
            .write(|w| w.ndt().bits(blocks.len() as u16 * 0x80));

        //    - Set the word size, direction and increments.
        self.dma.ccr4.write(|w| {
            w.dir()
                .set_bit()
                .minc()
                .set_bit()
                .pinc()
                .clear_bit()
                .msize()
                .bits32()
                .psize()
                .bits32()
        });

        //    - Enable the channel.
        self.dma.ccr4.modify(|_, w| w.en().set_bit());

        // c. Set the address.
        // d. Set the command register.
        self.card_command_short(Command::WRITE_MULTIPLE_BLOCK, address.0)?;
        self.state = State::Writing;

        // e. Set the data control register:
        self.sdmmc.dctrl.write(|w| unsafe {
            w.dten()
                .set_bit()
                .dtdir()
                .clear_bit()
                .dmaen()
                .set_bit()
                .dblocksize()
                .bits(0x9)
        });

        Ok(())
    }

    fn result(&mut self) -> nb::Result<(), Error> {
        let status = self.sdmmc.sta.read();
        match self.state {
            State::Uninitialized | State::Init1(_) => Err(Other(Error::Uninitialized)),
            State::Ready => panic!("called CardHost::result without starting an operation"),
            State::Reading if status.rxact().bit() => Err(WouldBlock),
            State::Writing if status.txact().bit() => Err(WouldBlock),
            State::Reading | State::Writing => Ok(()),
        }?;

        self.dma.ccr4.modify(|_, w| w.en().clear_bit());
        self.sdmmc
            .icr
            .write(|w| unsafe { w.bits(STATUS_ERROR_MASK) });
        self.state = State::Ready;
        if status.dcrcfail().bit() {
            Err(Other(CRCFail))
        } else if status.dtimeout().bit() {
            Err(Other(Timeout))
        } else if status.rxoverr().bit() {
            Err(Other(ReceiveOverrun))
        } else if status.txunderr().bit() {
            Err(Other(SendUnderrun))
        } else if !status.dataend().bit() || !status.dbckend().bit() {
            Err(Other(UnknownResult))
        } else {
            Ok(())
        }
    }
}
