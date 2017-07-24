use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::mpsc::{TryRecvError, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::collections::VecDeque;

use pocsag::{TimeSlots, Message, MessageProvider, Generator, TestGenerator};
use transmitter::{self, Transmitter};
use config::Config;

enum Command {
    Message(Message),
    SetTimeSlots(TimeSlots),
    Stop
}

#[derive(Clone)]
pub struct Scheduler {
    tx: Sender<Command>,
    scheduler: Arc<Mutex<SchedulerCore>>
}

struct SchedulerCore {
    rx: Receiver<Command>,
    slots: TimeSlots,
    queue: VecDeque<Message>,
    stop: bool,
    budget: usize
}

impl Scheduler {
    pub fn new(_: &Config) -> Scheduler {
        let (tx, rx) = channel();

        let core = SchedulerCore {
            rx: rx,
            slots: TimeSlots::new(),
            queue: VecDeque::new(),
            stop: false,
            budget: 0
        };

        Scheduler {
            tx: tx,
            scheduler: Arc::new(Mutex::new(core))
        }
    }

    pub fn start(config: Config, scheduler: Scheduler) -> JoinHandle<()> {
        thread::spawn(move || {
            let transmitter = transmitter::from_config(&config);
            scheduler.scheduler.lock().unwrap().run(transmitter);
        })
    }

    pub fn test(config: Config, scheduler: Scheduler) -> JoinHandle<()> {
        thread::spawn(move || {
            let transmitter = transmitter::from_config(&config);
            scheduler.scheduler.lock().unwrap().test(transmitter);
        })
    }

    pub fn set_time_slots(&self, slots: TimeSlots) -> bool {
        info!("Set {:?}", slots);
        status!(timeslots: slots);
        self.tx.send(Command::SetTimeSlots(slots)).is_ok()
    }

    pub fn message(&self, msg: Message) -> bool {
        info!("Received {:?}", msg);
        self.tx.send(Command::Message(msg)).is_ok()
    }

    pub fn stop(&self) -> bool {
        self.tx.send(Command::Stop).is_ok()
    }
}

impl SchedulerCore {
    pub fn run(&mut self, mut transmitter: Box<Transmitter>) {
        info!("Scheduler started.");
        while !self.stop {
            let mut message = self.queue.pop_front();

            while message.is_none() {
                match self.rx.recv() {
                    Ok(Command::Message(msg)) => {
                        message = Some(msg);
                    },
                    Ok(Command::SetTimeSlots(slots)) => {
                        self.slots = slots;
                    },
                    Ok(Command::Stop) => { return; },
                    Err(_) => { return; }
                }
            }

            status!(queue: self.queue.len() + 1);

            // Calculate remaining time budget
            self.budget = self.slots.calculate_budget();

            if self.budget > 30 { /* transmit immediately */ }
            else if let Some(next_slot) = self.slots.next_allowed() {
                let mut duration = next_slot.duration_until();

                debug!("Waiting {} seconds until {:?}...",
                       duration.as_secs(), next_slot);

                // Process other commands while waiting for the time slot
                'waiting: while !next_slot.active() {
                    duration = next_slot.duration_until();

                    match self.rx.recv_timeout(duration) {
                        Ok(Command::Message(msg)) => {
                            self.queue.push_back(msg);
                            status!(queue: self.queue.len() + 1);
                        },
                        Ok(Command::SetTimeSlots(slots)) => {
                            self.slots = slots;
                        },
                        Ok(Command::Stop) => { return; },
                        Err(RecvTimeoutError::Disconnected) => { return; }
                        Err(RecvTimeoutError::Timeout) => { break 'waiting; }
                    }
                }
            }
            else {
                warn!("No allowed time slots! Sending anyway...");
            }

            status!(queue: self.queue.len());
            status!(transmitting: true);
            self.budget = self.slots.calculate_budget();
            info!("Calculated Budget: {}", self.budget);
            transmitter.send(&mut Generator::new(self, message.unwrap()));
            status!(transmitting: false);
        }
    }

    pub fn test(&mut self, mut transmitter: Box<Transmitter>) {
        status!(transmitting: true);
        transmitter.send(&mut TestGenerator::new(1125));
        status!(transmitting: false);
    }
}

impl MessageProvider for SchedulerCore {
    fn next(&mut self, count: usize) -> Option<Message> {
        debug!("Remaining budget: {}", self.budget as i32 - count as i32);

        if count + 30 > self.budget {
            return None;
        }

        loop {
            match (*self).rx.try_recv() {
                Ok(Command::Message(msg)) =>{
                    self.queue.push_back(msg);
                }
                Ok(Command::SetTimeSlots(slots)) => {
                    self.slots = slots;
                },
                Ok(Command::Stop) => {
                    self.stop = true;
                    return None;
                },
                Err(TryRecvError::Disconnected) => {
                    self.stop = true;
                    return None;
                },
                Err(TryRecvError::Empty) => { break; }
            };
        }

        let message = self.queue.pop_front();
        status!(queue: self.queue.len());
        message
    }
}
