// Copyright 2020 David Young
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crossbeam_channel::unbounded;
use std::error::Error;
use std::fmt;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration; // multi-receiver channels

use rppal::gpio::Gpio;
use rppal::uart::{Parity, Uart};

//TODO
// * use cmri library for packet processing
// * three threads:
//   - TCP RX -> RS485 queue
//   - TCP TX <- RS485 queue
//   - RS485 handler:
//     * bus controller in half duplex mode, so all comms are initiated from here
//     - wait for incoming tcp message
//     - transmit the message over RS485
//     - if message type requires a response then switch to RX mode and wait
//         for either response or timeout
//     - send response down TCP TX queue
// * Two queues are needed, but need to be crossbeam as there is no design limit
//     on the number of simultaneous TCP sessions
// * configure TTY (auto-discovery?) and other params via CLI

const UART: &str = "/dev/ttyAMA1";
const BAUD_RATE: u32 = 19200;
const RTS_PIN: u8 = 11;
const PORT: u16 = 4000;
const CMRI_PREAMBLE_BYTE: u8 = 0xff;
const CMRI_START_BYTE: u8 = 0x02;
const CMRI_STOP_BYTE: u8 = 0x03;
const CMRI_ESCAPE_BYTE: u8 = 0x10;
// number of byte-lengths extra to wait to account for delays
const EXTRA_TX_TIME: u64 = 4;

#[derive(Copy, Clone, Debug)]
enum CmriState {
    Idle,
    Attn,
    Start,
    Addr,
    Type,
    Data,
    Escape,
}

#[derive(Clone, Debug)]
struct CmriPacket {
    payload: Vec<u8>,
    state: CmriState,
}

impl CmriPacket {
    fn new() -> Self {
        let mut s = Self {
            // capacity is the max length from
            // https://github.com/madleech/ArduinoCMRI/blob/master/CMRI.h
            payload: Vec::with_capacity(258),
            state: CmriState::Idle,
        };
        s.append(&mut [255_u8, 255]);
        s
    }

    /// Try to interpret the next byte read in. Returns an Err(CmriState)
    /// with the current state if the message is still happening or an Ok(())
    /// if the message is complete to suggest to the processor that it might
    /// want to process the full packet.
    fn try_push(&mut self, byte: u8) -> Result<(), CmriState> {
        use CmriState::*;
        match self.state {
            Idle => {
                // Idle to Attn if byte is PREAMBLE
                if byte == CMRI_PREAMBLE_BYTE {
                    self.payload.clear();
                    self.payload.push(byte);
                    self.state = Attn;
                }
                // Ignore other bytes while Idle
            }
            Attn => {
                // Attn to Start if byte is PREAMBLE
                if byte == CMRI_PREAMBLE_BYTE {
                    self.payload.push(byte);
                    self.state = Start;
                } else {
                    // Otherwise discard and reset to Idle
                    self.payload.clear();
                    self.state = Idle;
                }
            }
            Start => {
                // start byte must be valid
                if byte == CMRI_START_BYTE {
                    self.payload.push(byte);
                    self.state = Addr;
                } else {
                    // Otherwise discard and reset to Idle
                    self.payload.clear();
                    self.state = Idle;
                }
            }
            Addr => {
                // Take the next byte as-is for an address
                self.payload.push(byte);
                self.state = Type;
            }
            Type => {
                // Take the next byte as-is for message type
                self.payload.push(byte);
                self.state = Data;
            }
            Data => {
                match byte {
                    CMRI_ESCAPE_BYTE => {
                        // escape the next byte
                        self.payload.push(byte);
                        self.state = Escape;
                    }
                    CMRI_STOP_BYTE => {
                        // end transmission
                        self.payload.push(byte);
                        self.state = Idle;
                        return Ok(());
                    }
                    _ => {
                        // any other byte we take as data
                        self.payload.push(byte);
                    }
                }
            }
            Escape => {
                // Escape the next byte, so accept it as data.
                self.payload.push(byte);
                self.state = Data;
            }
        }
        Err(self.state)
    }

    fn append(&mut self, buf: &mut [u8]) {
        self.payload.extend_from_slice(buf);
    }

    fn len(&self) -> usize {
        self.payload.len()
    }
}

impl fmt::Display for CmriPacket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.payload)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("Hello, world!");

    // there will only be one receiver on the uart end
    let (tcp_to_485_tx, tcp_to_485_rx): (
        mpsc::Sender<CmriPacket>,
        mpsc::Receiver<CmriPacket>,
    ) = mpsc::channel();

    // it cannot be known how many tcp receivers will exist, hence crossbeam
    let (rs485_to_tcp_tx, rs485_to_tcp_rx) = unbounded();

    thread::spawn(move || start_listener(tcp_to_485_tx, rs485_to_tcp_rx));

    let mut rts_pin = Gpio::new()?.get(RTS_PIN)?.into_output();
    rts_pin.set_low(); // set RTS high to put MAX485 into RX mode
    let mut uart = Uart::with_path(UART, BAUD_RATE, Parity::None, 8, 2)?;
    let mut buffer = [0_u8];
    let mut rx_packet = CmriPacket::new();

    // 8 bits * microseconds * seconds per bit
    let byte_time = (8_f64 * 1_000_000_f64 * 1_f64 / (BAUD_RATE as f64)) as u64;

    rts_pin.set_high();
    uart.drain()?;
    rts_pin.set_low();
    loop {
        // Handle all of the UART stuff
        // Check the mpsc in case there is a packet to transmit
        match tcp_to_485_rx.try_recv() {
            Ok(packet) => {
                // send the packet out of the uart
                println!("Sending down uart");
                rts_pin.set_high();
                uart.write(&packet.payload)?; // default non-blocking
                thread::sleep(Duration::from_micros(
                    (EXTRA_TX_TIME + packet.len() as u64) * byte_time,
                )); // wait until all data transmitted
                println!("Output queue len is {}", uart.output_len()?);
                uart.drain()?;
                rts_pin.set_low();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                println!("A bad");
                break;
            }
        }

        // Try reading the uart. If a byte is successfully read then we try
        // to read more bytes and hopefully end up with a complete message
        if uart.read(&mut buffer)? > 0 {
            loop {
                #[allow(clippy::single_match)]
                match rx_packet.try_push(buffer[0]) {
                    Ok(()) => {
                        // Got a full packet
                        println!("Received uart packet: {:?}", rx_packet);
                        rs485_to_tcp_tx.send(rx_packet.clone()).unwrap();
                        break;
                    }
                    Err(_) => {} // Still receiving
                }
                buffer = [0];

                if uart.read(&mut buffer)? == 0 {
                    println!("No more data to read");
                    break;
                }
            }
        }

        thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

fn start_listener(
    tcp_to_485_tx: mpsc::Sender<CmriPacket>,
    rs485_to_tcp_rx: crossbeam_channel::Receiver<CmriPacket>,
) {
    let listener = TcpListener::bind(format!("[::1]:{}", PORT)).unwrap();
    println!("Server listening on port {}", PORT);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                println!("Connection from {}", stream.peer_addr().unwrap());
                let sender = tcp_to_485_tx.clone();
                let receiver = rs485_to_tcp_rx.clone();
                // Clone the stream handle so that it can be accessed
                // by two threads
                let stream_sender = stream.try_clone().unwrap();

                // Start thread that receives from tcp and sends to uart
                thread::spawn(move || tcp_rx(stream, sender));

                // start thread that receives from uart channel and sends
                // to tcp
                thread::spawn(move || tcp_tx(stream_sender, receiver));
            }
            Err(e) => {
                println!("Connection failed with error \"{}\"", e);
            }
        }
    }
    drop(listener);
}

fn tcp_rx(mut stream: TcpStream, tx_channel: mpsc::Sender<CmriPacket>) {
    let mut buf = [0_u8; 1];
    let mut packet: CmriPacket = CmriPacket::new();
    loop {
        // try reading a byte off the stream
        match stream.read(&mut buf) {
            Ok(1) => {
                // got a single byte
                match packet.try_push(buf[0]) {
                    Err(_) => {} // still listening
                    Ok(()) => {
                        // message is complete!!
                        println!("Received TCP message {:?}", packet);
                        tx_channel.send(packet.clone()).unwrap();
                    }
                }
            }
            Err(e) => {
                println!("Read failed: {}", e);
                stream.shutdown(Shutdown::Both).unwrap();
                break;
            }
            _ => {}
        }
    }
    println!("client exited");
}

fn tcp_tx(
    mut stream: TcpStream,
    rx_channel: crossbeam_channel::Receiver<CmriPacket>,
) {
    loop {
        // if there is data to send then send it
        match rx_channel.recv() {
            //Err(RecvError::Empty) => {} // do nothing
            Err(e) => {
                println!("Receive error: {}", e);
                stream.shutdown(Shutdown::Both).unwrap();
                break;
            }
            Ok(msg) => {
                // got a packet, let's send it!
                println!("Forwarding packet to TCP stream");
                if let Err(e) = stream.write_all(&msg.payload) {
                    println!("Unable to write payload! Error: {}", e);
                    break;
                }
            }
        }
    }
    println!("Client exited");
}
