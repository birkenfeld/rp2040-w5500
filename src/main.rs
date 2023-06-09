//! Raspberry Pi Pico and Wiznet W5500 experiment.
//!
//! Mostly copied from https://github.com/newAM/ambientsensor-rs
#![no_std]
#![no_main]

use defmt::*;
use defmt_rtt as _;
use panic_probe as _;

use core::sync::atomic::{compiler_fence, Ordering::SeqCst};
use embedded_hal::digital::v2::OutputPin;
use fugit::{ExtU64, RateExtU32};
use rp_pico::hal::{
    self,
    gpio::{Input, Interrupt, Output, Pin, PullDown, PushPull},
    pac::SPI1,
    spi::{Enabled, Spi},
    Clock,
};
use systick_monotonic::Systick;
use w5500_dhcp::{
    hl::{Common, Hostname, Tcp},
    ll::{
        eh0::{reset, vdm_infallible_gpio::W5500, MODE as W5500_MODE},
        net::Eui48Addr,
        LinkStatus, OperationMode, PhyCfg, Registers, Sn, SocketInterruptMask,
    },
    Client as DhcpClient,
};

/// External high-speed crystal on the Raspberry Pi Pico board is 12 MHz.
const XTAL_FREQ_HZ: u32 = 12_000_000u32;

const DHCP_SN: Sn = Sn::Sn0;
const SECOP_SN: Sn = Sn::Sn1;

const NAME: &str = "pinode";
const HOSTNAME: Hostname<'static> = Hostname::new_unwrapped(NAME);
const SECOP_PORT: u16 = 10767;

fn monotonic_secs() -> u32 {
    app::monotonics::now()
        .duration_since_epoch()
        .to_secs()
        .try_into()
        .unwrap()
}

#[rtic::app(
    device = crate::hal::pac,
    dispatchers = [UART0_IRQ, UART1_IRQ],
)]
mod app {
    use super::*;

    // RTIC manual says not to use this in production.
    #[monotonic(binds = SysTick, default = true)]
    type MyMono = Systick<10>; // 10 Hz / 0.1 s granularity

    #[shared]
    struct Shared {
        w5500: W5500<Spi<Enabled, SPI1, 8>, Pin<hal::gpio::bank0::Gpio13, Output<PushPull>>>,
        dhcp: DhcpClient<'static>,
        dhcp_spawn_at: Option<u32>,
    }

    #[local]
    struct Local {
        irq_pin: Pin<hal::gpio::bank0::Gpio14, Input<PullDown>>,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local, init::Monotonics) {
        // Setup clocks and the watchdog.
        let mut dp = cx.device;
        let mut watchdog = hal::watchdog::Watchdog::new(dp.WATCHDOG);
        let clocks = hal::clocks::init_clocks_and_plls(
            XTAL_FREQ_HZ,
            dp.XOSC,
            dp.CLOCKS,
            dp.PLL_SYS,
            dp.PLL_USB,
            &mut dp.RESETS,
            &mut watchdog,
        )
        .ok()
        .unwrap();

        // Setup the pins.
        let sio = hal::sio::Sio::new(dp.SIO);
        let pins = hal::gpio::Pins::new(dp.IO_BANK0, dp.PADS_BANK0, sio.gpio_bank0, &mut dp.RESETS);

        let systick = cx.core.SYST;
        let mut delay = cortex_m::delay::Delay::new(systick, clocks.system_clock.freq().to_Hz());

        let w5500_cs = pins.gpio13.into_push_pull_output();
        let mut w5500_rst = pins.gpio15.into_push_pull_output();
        let w5500_int = pins.gpio14.into_pull_down_input();
        w5500_int.set_interrupt_enabled(Interrupt::EdgeLow, true);

        let _spi_sclk = pins.gpio10.into_mode::<hal::gpio::FunctionSpi>();
        let _spi_mosi = pins.gpio11.into_mode::<hal::gpio::FunctionSpi>();
        let _spi_miso = pins.gpio12.into_mode::<hal::gpio::FunctionSpi>();
        let spi1 = hal::spi::Spi::<_, _, 8>::new(dp.SPI1);
        let spi1 = spi1.init(
            &mut dp.RESETS,
            clocks.peripheral_clock.freq(),
            1_000_000u32.Hz(),
            &W5500_MODE,
        );

        let mut w5500 = W5500::new(spi1, w5500_cs);

        info!("Initialized");

        let mac = Eui48Addr::new(0x46, 0x52, 0x4d, 0x01, 0x02, 0x03);

        reset(&mut w5500_rst, &mut delay).unwrap();

        // continually initialize the W5500 until we link up
        let _phy_cfg: PhyCfg = 'outer: loop {
            // sanity check W5500 communications
            core::assert_eq!(w5500.version().unwrap(), w5500_dhcp::ll::VERSION);

            // load the MAC address
            w5500.set_shar(&mac).unwrap();
            core::debug_assert_eq!(w5500.shar().unwrap(), mac);

            // wait for the PHY to indicate the Ethernet link is up
            let mut attempts: u32 = 0;
            info!("Polling for link up");
            const PHY_CFG: PhyCfg = PhyCfg::DEFAULT.set_opmdc(OperationMode::Auto);
            w5500.set_phycfgr(PHY_CFG).unwrap();

            const LINK_UP_POLL_PERIOD_MILLIS: u32 = 100;
            const LINK_UP_POLL_ATTEMPTS: u32 = 50;
            loop {
                let phy_cfg: PhyCfg = w5500.phycfgr().unwrap();
                if phy_cfg.lnk() == LinkStatus::Up {
                    break 'outer phy_cfg;
                }
                if attempts >= LINK_UP_POLL_ATTEMPTS {
                    info!(
                        "Failed to link up in {} ms",
                        attempts * LINK_UP_POLL_PERIOD_MILLIS,
                    );
                    break;
                }
                delay.delay_ms(LINK_UP_POLL_PERIOD_MILLIS);
                attempts += 1;
            }

            w5500_rst.set_low().unwrap();
            delay.delay_ms(1);
            w5500_rst.set_high().unwrap();
            delay.delay_ms(3);
        };
        info!("Done link up");

        let seed: u64 = u64::from(cortex_m::peripheral::SYST::get_current()) << 32
            | u64::from(cortex_m::peripheral::SYST::get_current());

        // additional delay seems to be required until DHCP request can be sent
        delay.delay_ms(500);

        let dhcp = DhcpClient::new(DHCP_SN, seed, mac, HOSTNAME);
        dhcp.setup_socket(&mut w5500).unwrap();

        let simr = w5500.simr().unwrap();
        w5500.set_simr(simr | SECOP_SN.bitmask()).unwrap();
        const MASK: SocketInterruptMask = SocketInterruptMask::ALL_MASKED
            .unmask_recv().unmask_con().unmask_discon();
        w5500.close(SECOP_SN).unwrap();
        w5500.set_sn_imr(SECOP_SN, MASK).unwrap();
        w5500.tcp_listen(SECOP_SN, SECOP_PORT).unwrap();

        // start the DHCP client
        dhcp_task::spawn().unwrap();

        // start the timeout tracker
        timeout_tracker::spawn().unwrap();

        // use systick for monotonic clock now
        let mono = Systick::new(delay.free(), clocks.system_clock.freq().to_Hz());

        (
            Shared {
                w5500,
                dhcp,
                dhcp_spawn_at: None,
            },
            Local { irq_pin: w5500_int },
            init::Monotonics(mono),
        )
    }

    #[idle]
    fn idle(_: idle::Context) -> ! {
        loop {
            compiler_fence(SeqCst);
        }
    }

    /// IRQ handler for the W5500, dispatching tasks by socket
    #[task(binds = IO_IRQ_BANK0, local = [irq_pin], shared = [w5500])]
    fn irq_bank0(mut cx: irq_bank0::Context) {
        debug!("[W5500] got interrupt");

        cx.shared.w5500.lock(|w5500| {
            let sir: u8 = w5500.sir().unwrap();

            cx.local.irq_pin.clear_interrupt(Interrupt::EdgeLow);

            // may occur when there are power supply issues
            if sir == 0 {
                warn!("[W5500] spurious interrupt");
                return;
            }

            if sir & DHCP_SN.bitmask() != 0 {
                if dhcp_task::spawn().is_err() {
                    error!("DHCP task already spawned")
                }
            }

            if sir & SECOP_SN.bitmask() != 0 {
                if secop_task::spawn().is_err() {
                    error!("SECOP task already spawned")
                }
            }
        });
    }

    /// Task to spawn DHCP actions when necessary
    #[task(shared = [dhcp_spawn_at])]
    fn timeout_tracker(mut cx: timeout_tracker::Context) {
        timeout_tracker::spawn_after(1.secs()).unwrap();

        let now: u32 = monotonic_secs();

        cx.shared.dhcp_spawn_at.lock(|dhcp_spawn_at| {
            if let Some(then) = dhcp_spawn_at {
                if now >= *then {
                    if dhcp_task::spawn().is_err() {
                        error!("DHCP task already spawned")
                    }
                    *dhcp_spawn_at = None;
                }
            }
        });

        secop_task::spawn().ok();
    }

    /// DHCP client task
    #[task(shared = [w5500, dhcp, dhcp_spawn_at])]
    fn dhcp_task(cx: dhcp_task::Context) {
        (cx.shared.w5500, cx.shared.dhcp, cx.shared.dhcp_spawn_at).lock(
            |w5500, dhcp, dhcp_spawn_at| {
                let now = monotonic_secs();
                let spawn_after_secs = match dhcp.process(w5500, now) {
                    Ok(sec) => sec,
                    Err(e) => {
                        error!("[DHCP] error {}", e);
                        5
                    }
                };

                let spawn_at = now + spawn_after_secs;
                *dhcp_spawn_at = Some(spawn_at);
                info!("[DHCP] spawning after {} seconds, at {}",
                      spawn_after_secs, spawn_at);
            },
        )
    }

    /// SECoP server task
    #[task(shared = [w5500])]
    fn secop_task(cx: secop_task::Context) {
        (cx.shared.w5500,).lock(
            |w5500| {
                let mut buf = [0; 256];
                let rx_bytes: u16 = w5500.tcp_read(SECOP_SN, &mut buf).unwrap();
                if rx_bytes > 0 {
                    info!("got {} bytes from secop: {}", rx_bytes, &buf[..rx_bytes as usize]);
                    w5500.tcp_write(SECOP_SN, &buf[..rx_bytes as usize]).unwrap();
                }
            }
        )
    }
}
