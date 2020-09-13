# pi-cmri-proxy

<!-- ![Build](https://github.com/sciguy16/cmri/workflows/Build/badge.svg?branch=main) -->

# Rewrite to use the cmri library is in progress

Userland service that runs on a Raspberry Pi and acts as a proxy between JMRI on TCP and a C/MRI network running over RS485. The need for this came about because half-duplex RS485 requires a control signal to toggle the transceiver between transmit and receive mode, which was proving difficult to achieve otherwise.

## Usage
Starting the program with `cargo run --release` will start a TCP listener on localhost port 4000

## Installation and removal
There are two handy scripts - `install.sh` and `uninstall.sh`. The installation script checks that Cargo is installed and then compiles the binary and copies it to `/usr/local/bin`. It then installs `ip_to_485` as a SystemD service to run at boot and starts it off.

Similarly, `uninstall.sh` stops and deregisters the SystemD service and removes the program from `/usr/local/bin`.

## Test setup
During development, a Pi 4 was connected to a MAX485 (with level shift divider on the RX side) and two arduino nano clones running a simple program based on [ArduinoCMRI](https://github.com/madleech/ArduinoCMRI) that provides a single sensor and single light.

The transceiver was wired to UART4, corresponding to GPIO pins 8 and 9, with the direction toggle connected to RTS4 on GPIO 11.

The following photos show the test setup:

![Raspberry Pi 4 with MAX485 transceiver](images/pi.jpg)

![Two Arduinos running as CMRI nodes](images/arduino.jpg)

Finally, here is a scope trace showing an incoming CMRI packet followed by the next CMRI poll. The top trace is RS485-A and the bottom is RTS:

![CMRI packets over RS485](images/scope_trace.jpg)

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
