use std::time::Duration;

use rppal::gpio::{Gpio, InputPin, Level, Trigger};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Set up the switch input pin with a pull-up resistor and async interrupt.
///
/// Returns:
/// - The `InputPin` (must be kept alive for interrupts to fire)
/// - A `watch::Sender<bool>` — clone this to add more producers (e.g. the
///   software control socket) that can override the switch state
/// - A `watch::Receiver<bool>` that yields `true` when the switch is active
///   (pin HIGH — switch open) and `false` when inactive (pin LOW — switch to ground)
pub fn setup_switch(
    pin_number: u8,
) -> Result<(InputPin, watch::Sender<bool>, watch::Receiver<bool>), rppal::gpio::Error> {
    let gpio = Gpio::new()?;
    let mut pin = gpio.get(pin_number)?.into_input_pullup();

    let initial = pin.read() == Level::High;
    let (tx, rx) = watch::channel(initial);
    // Clone the sender for the interrupt closure; the original is returned to the caller.
    let tx_interrupt = tx.clone();

    pin.set_async_interrupt(Trigger::Both, None, move |event| {
        tracing::info!("Switch event: {event:#?}");
        // RisingEdge = pin went HIGH (switch opened = active)
        // FallingEdge = pin went LOW (switch closed to ground = inactive)
        let active = event.trigger == Trigger::RisingEdge;
        // Ignore send errors — receiver may have been dropped during shutdown
        let _ = tx_interrupt.send(active);
    })?;

    Ok((pin, tx, rx))
}

/// Blink a LED on `pin_number` until the `CancellationToken` is cancelled,
/// then leave the LED in the given `final_state`.
pub async fn blink_led(
    pin_number: u8,
    blink_interval: Duration,
    cancel: CancellationToken,
    final_state: bool,
) -> Result<(), rppal::gpio::Error> {
    let gpio = Gpio::new()?;
    let mut pin = gpio.get(pin_number)?.into_output_low();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(blink_interval) => {
                pin.toggle();
            }
        }
    }

    if final_state {
        pin.set_high();
    } else {
        pin.set_low();
    }

    Ok(())
}

/// Drive the LED to a fixed level (non-blinking).
pub fn set_led(pin_number: u8, on: bool) -> Result<(), rppal::gpio::Error> {
    let gpio = Gpio::new()?;
    let mut pin = gpio.get(pin_number)?.into_output();
    if on {
        pin.set_high();
    } else {
        pin.set_low();
    }
    Ok(())
}
