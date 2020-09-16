// Copyright 2020 David Young
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use crossbeam_channel::unbounded;
use std::error::Error;

use cmri::{CmriMessage, CmriStateMachine, RxState, TX_BUFFER_LEN};
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

// number of byte-lengths extra to wait to account for delays
const EXTRA_TX_TIME: u64 = 4;

fn main() -> Result<(), Box<dyn Error>> {
    // there will only be one receiver on the uart end
    let (tcp_to_485_tx, tcp_to_485_rx): (mpsc::Sender<CmriMessage>, mpsc::Receiver<CmriMessage>) =
        mpsc::channel();

    // it cannot be known how many tcp receivers will exist, hence crossbeam
    let (rs485_to_tcp_tx, rs485_to_tcp_rx) = unbounded();

    thread::spawn(move || start_listener(tcp_to_485_tx, rs485_to_tcp_rx));

    let mut rts_pin = Gpio::new()?.get(RTS_PIN)?.into_output();
    rts_pin.set_low(); // set RTS high to put MAX485 into RX mode
    let mut uart = Uart::with_path(UART, BAUD_RATE, Parity::None, 8, 2)?;
    let mut buffer = [0_u8];
    let mut state = CmriStateMachine::new();

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
                                              //thread::sleep(Duration::from_micros(
                                              //    (EXTRA_TX_TIME + packet.len() as u64) * byte_time,
                                              //)); // wait until all data transmitted
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
                match state.process(buffer[0]) {
                    Ok(RxState::Complete) => {
                        // Got a full packet
                        println!("Received uart packet: {:?}", state.message().message_type);
                        rs485_to_tcp_tx.send(state.message().clone()).unwrap();
                        break;
                    }
                    Ok(RxState::Listening) => {}
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
    tcp_to_485_tx: mpsc::Sender<CmriMessage>,
    rs485_to_tcp_rx: crossbeam_channel::Receiver<CmriMessage>,
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

fn tcp_rx(mut stream: TcpStream, tx_channel: mpsc::Sender<CmriMessage>) {
    let mut buf = [0_u8; 1];
    let mut state = CmriStateMachine::new();
    loop {
        // try reading a byte off the stream
        match stream.read(&mut buf) {
            Ok(1) => {
                // got a single byte
                match state.process(buf[0]) {
                    Err(_) => {} // still listening
                    Ok(RxState::Listening) => {}
                    Ok(RxState::Complete) => {
                        // message is complete!!
                        println!("Received TCP message {:?}", state.message().message_type);
                        tx_channel.send(state.message().clone()).unwrap();
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

fn tcp_tx(mut stream: TcpStream, rx_channel: crossbeam_channel::Receiver<CmriMessage>) {
    let mut buf = [0_u8; TX_BUFFER_LEN];
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
                if let Err(e) = msg.encode(&mut buf) {
                    println!("Error encoding message: {}", e);
                }
                if let Err(e) = stream.write_all(&buf) {
                    println!("Unable to write payload! Error: {}", e);
                    break;
                }
            }
        }
    }
    println!("Client exited");
}
