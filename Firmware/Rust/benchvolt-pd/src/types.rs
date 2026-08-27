//! Concrete aliases for the firmware's fully-instantiated runtime types, so
//! the loop modules can take plain `&mut` parameters instead of re-spelling
//! pin generics everywhere.

use benchvolt_pd::app::AppReducer;
use benchvolt_pd::power::{FirmwareEffectPlanner, PowerExecutor};
use reducto::EffectApp;
use stm32f0xx_hal::gpio::{
    gpioa::{PA8, PA9},
    gpioc::{PC6, PC7, PC8, PC9},
    OpenDrain, Output,
};

use crate::board::{i2c::SoftI2c, power::HardwarePowerDriver};
use crate::display_dma::QueuedDisplay;
use crate::view::BenchVoltView;

pub(crate) type FirmwareDriver = HardwarePowerDriver<
    PC8<Output<OpenDrain>>,
    PC9<Output<OpenDrain>>,
    PC6<Output<OpenDrain>>,
    PC7<Output<OpenDrain>>,
>;

pub(crate) type FirmwarePower = PowerExecutor<FirmwareDriver>;

pub(crate) type FirmwareApp =
    EffectApp<AppReducer, BenchVoltView<QueuedDisplay>, FirmwareEffectPlanner, 8>;

pub(crate) type PdI2c = SoftI2c<
    PA8<Output<OpenDrain>>,
    PA9<Output<OpenDrain>>,
    { benchvolt_pd::pd::STUSB4500_I2C_HALF_CYCLE_US },
>;
