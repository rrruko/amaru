// Copyright 2025 PRAGMA
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Spawn one pipeline for each node in the test;
// Generate client requests (and faults) with random arrival times and insert them in heap of messages to be delivered (the heap is ordered by arrival time);
// Pop the next client request from the heap of messages;
// Advance the time to the arrival time, of the popped message, on all nodes, potentially triggering timeouts;
// Call get_state to dump the current/pre-state on the receiving node;
// Deliver the message the receiving enqueue_msg (unless there's some network fault stopping it);
// Process the message using run_until_blocked and drain.collect all outgoing messages (storage effects will later have to be dealt with here as well);
// Dump the post-state and append the pre-state, post-state, incoming message and outgoing messages to the simulator's "trace";
// Assign random arrival times for the outgoing messages (this creates different message interleavings) and insert them back into the heap;
// Go to 3 and continue until heap is empty;
// Make assertions on the trace to ensure the execution was correct, if not, shrink and present minimal trace that breaks the assertion together with the seed that allows us to reproduce the execution.

use crate::echo::{EchoMessage, Envelope};
use pure_stage::simulation::{Receiver, SimulationRunning};
use pure_stage::{StageRef, Void};

use anyhow::anyhow;
use proptest::{
    prelude::*,
    test_runner::{Config, TestError, TestRunner},
};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    fmt::Debug,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Entry<Msg> {
    arrival_time: Instant,
    envelope: Envelope<Msg>,
}

impl<Msg: PartialEq> PartialOrd for Entry<Msg> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Msg: PartialEq> Ord for Entry<Msg> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.arrival_time.cmp(&other.arrival_time)
    }
}

impl<Msg: PartialEq> Eq for Entry<Msg> {}

type NodeId = String;

pub struct NodeHandle {
    handle:
        Box<dyn FnMut(Envelope<EchoMessage>) -> Result<Vec<Envelope<EchoMessage>>, anyhow::Error>>,
    close: Box<dyn FnMut()>,
}

#[allow(unused)]
pub fn pure_stage_node_handle(
    mut rx: Receiver<Envelope<EchoMessage>>,
    stage: StageRef<Envelope<EchoMessage>, (u64, StageRef<Envelope<EchoMessage>, Void>)>,
    mut running: SimulationRunning,
) -> anyhow::Result<NodeHandle> {
    let handle = Box::new(move |msg: Envelope<EchoMessage>| {
        running.enqueue_msg(&stage, [msg]);
        running.run_until_blocked().assert_idle();
        Ok(rx.drain().collect::<Vec<_>>())
    });

    let close = Box::new(move || ());

    Ok(NodeHandle { handle, close })
}

#[allow(unused)]
pub fn pipe_node_handle(filepath: &Path, args: &[&str]) -> anyhow::Result<NodeHandle> {
    let mut child = Command::new(filepath)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to create process: {}", e))?;
    let mut stdin = child.stdin.take().ok_or(anyhow!("Failed to take stdin"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(anyhow!("Failed to take stdout"))?;

    let handle = Box::new(move |msg: Envelope<EchoMessage>| {
        let json =
            serde_json::to_string(&msg).map_err(|e| anyhow!("Failed to encode JSON: {}", e))?;
        println!("About to write: {}", json);
        writeln!(stdin, "{}", json)
            .map_err(|e| anyhow!("Failed to write to child's stdin: {}", e))?;
        stdin
            .flush()
            .map_err(|e| anyhow!("Failed to flush child's stdin: {}", e))?;

        let mut reader = BufReader::new(&mut stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| anyhow!("Failed to read from child's stdout: {}", e))?;

        println!("Just read: {}", &line);
        serde_json::from_str(&line)
            // TODO: Read more than one message? Either make SUT send one message
            // per line and end by a termination token, or make write a JSON array
            // of messages?
            .map(|msg: Envelope<EchoMessage>| vec![msg])
            .map_err(|e| anyhow!("Failed to decode JSON: {}", e))
    });

    let close = Box::new(move || {
        child
            .kill()
            .map_err(|e| anyhow!("Failed to terminate process: {}", e))
            .ok();
    });

    Ok(NodeHandle { handle, close })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trace(pub Vec<Envelope<EchoMessage>>);

#[derive(Debug, PartialEq)]
pub enum Next {
    Done,
    Continue,
}

pub struct World {
    heap: BinaryHeap<Reverse<Entry<EchoMessage>>>,
    nodes: BTreeMap<NodeId, NodeHandle>,
    trace: Trace,
}

#[allow(dead_code)]
impl World {
    pub fn new(
        initial_messages: Vec<Reverse<Entry<EchoMessage>>>,
        node_handles: Vec<(NodeId, NodeHandle)>,
    ) -> Self {
        World {
            heap: BinaryHeap::from(initial_messages),
            nodes: node_handles.into_iter().collect(),
            trace: Trace(Vec::new()),
        }
    }

    /// Simulate a 'World' of interconnected nodes
    /// see https://github.com/pragma-org/simulation-testing/blob/main/blog/dist/04-simulation-testing-main-loop.md
    pub fn step_world(&mut self) -> Next {
        match self.heap.pop() {
            Some(Reverse(Entry {
                arrival_time,
                envelope,
            })) =>
            // TODO: deal with time advance across all nodes
            // eg. run all nodes whose next action is ealier than msg's arrival time
            // and enqueue their output messages possibly bailing out and recursing
            {
                match self.nodes.get_mut(&envelope.dest) {
                    Some(node) => match (node.handle)(envelope.clone()) {
                        Ok(outgoing) => {
                            let (client_responses, outputs): (
                                Vec<Envelope<EchoMessage>>,
                                Vec<Envelope<EchoMessage>>,
                            ) = outgoing
                                .into_iter()
                                .partition(|msg| msg.dest.starts_with("c"));
                            outputs
                                .iter()
                                .map(|envelope| Entry {
                                    arrival_time: arrival_time + Duration::from_millis(100),
                                    envelope: envelope.clone(),
                                })
                                .for_each(|msg| self.heap.push(Reverse(msg)));
                            if envelope.src.starts_with("c") {
                                self.trace.0.push(envelope);
                            }
                            client_responses
                                .iter()
                                .for_each(|msg| self.trace.0.push(msg.clone()));
                            Next::Continue
                        }
                        Err(err) => panic!("{}", err),
                    },
                    None => panic!("unknown destination node '{}'", envelope.dest),
                }
            }
            None => Next::Done,
        }
    }

    pub fn run_world(&mut self) -> &[Envelope<EchoMessage>] {
        let prev = self.trace.0.len();
        while self.step_world() == Next::Continue {}
        &self.trace.0[prev..]
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.nodes
            .values_mut()
            .for_each(|node_handle| (node_handle.close)());
    }
}

#[allow(dead_code)]
pub fn simulate(
    config: Config,
    number_of_nodes: u8,
    spawn: fn() -> NodeHandle,
    generate_message: impl Strategy<Value = EchoMessage>,
    property: fn(Trace) -> Result<(), String>,
) {
    let mut runner = TestRunner::new(config);
    let generate_messages = prop::collection::vec(
        generate_message.prop_map(|msg| {
            Reverse(Entry {
                arrival_time: Instant::now(),
                envelope: Envelope {
                    src: "c1".to_string(),
                    dest: "n1".to_string(),
                    body: msg,
                },
            })
        }),
        0..20,
    );
    let result = runner.run(&generate_messages, |initial_messages| {
        let node_handles: Vec<_> = (1..=number_of_nodes)
            .map(|i| (format!("n{}", i), spawn()))
            .collect();

        let mut world = World::new(initial_messages, node_handles);
        let trace = world.run_world();

        match property(Trace(trace.to_vec())) {
            Ok(()) => (),
            Err(reason) => prop_assert!(false, "{}", reason),
        }
        Ok(())
    });
    match result {
        Ok(_) => (),
        Err(TestError::Fail(what, entries)) => {
            let mut err = String::new();
            entries
                .into_iter()
                .for_each(|entry| err += &format!("  {:?}\n", entry.0.envelope));
            panic!(
                "Found minimal failing case:\n\n{}\nError message:\n\n  {}",
                err, what
            )
        }
        Err(TestError::Abort(e)) => panic!("Test aborted: {}", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pure_stage::{simulation::SimulationBuilder, StageGraph, StageRef};

    #[test]
    fn run_stops_when_no_message_to_process_is_left() {
        let mut world = World::new(Vec::new(), Vec::new());

        assert_eq!(world.run_world(), &Vec::new());
    }

    #[test]
    #[should_panic]
    fn simulate_pure_stage_echo() {
        let config = Config::default();

        let number_of_nodes = 1;

        let spawn: fn() -> NodeHandle = || {
            println!("*** Spawning node!");
            let mut network = SimulationBuilder::default();
            let stage = network.stage(
                "echo",
                async |(mut state, out), msg: Envelope<EchoMessage>, eff| {
                    if let EchoMessage::Echo { msg_id, echo } = &msg.body {
                        state += 1;
                        // Insert a bug every 5 messages.
                        let echo_response = if state % 5 == 0 {
                            echo.to_string().to_uppercase()
                        } else {
                            echo.to_string()
                        };
                        let reply = Envelope {
                            src: msg.dest,
                            dest: msg.src,
                            body: EchoMessage::EchoOk {
                                msg_id: state,
                                in_reply_to: *msg_id,
                                echo: echo_response,
                            },
                        };
                        println!(" ==> {:?}", reply);
                        eff.send(&out, reply).await;
                        Ok((state, out))
                    } else {
                        panic!("Got a message that wasn't an echo: {:?}", msg.body)
                    }
                },
                (0u64, StageRef::noop::<Envelope<EchoMessage>>()),
            );
            let (output, rx) = network.output("output");
            let stage = network.wire_up(stage, |state| state.1 = output.without_state());
            let running = network.run();

            pure_stage_node_handle(rx, stage, running).unwrap()
        };
        let generate_message = (0..128u8).prop_map(|i| EchoMessage::Echo {
            msg_id: 0,
            echo: format!("Please echo {}", i),
        });
        simulate(
            config,
            number_of_nodes,
            spawn,
            generate_message,
            ECHO_PROPERTY,
        )
    }

    // TODO: Take response time into account.
    const ECHO_PROPERTY: fn(Trace) -> Result<(), String> = |trace: Trace| {
        for (index, msg) in trace
            .0
            .iter()
            .enumerate()
            .filter(|(_index, msg)| msg.src.starts_with("c"))
        {
            if let EchoMessage::Echo { msg_id, echo } = &msg.body {
                let response = trace.0.split_at(index + 1).1.iter().find(|resp| {
                        resp.dest == msg.src
                            && matches!(&resp.body, EchoMessage::EchoOk { in_reply_to, echo: resp_echo, .. }
                                if in_reply_to == msg_id && resp_echo == echo)
                    });
                if response.is_none() {
                    let mut err = String::new();
                    err += &format!(
                        "No matching response found for echo request:\n    {:?}\n\nTrace:\n",
                        msg
                    );
                    for envelope in trace.0 {
                        err += &format!("  {envelope:?}\n");
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    };

    // This shows how we can test external binaries. The test is disabled because building and
    // locating a binary on CI, across all platforms, is annoying.
    #[allow(dead_code)]
    #[ignore]
    fn blackbox_test_echo() {
        let config = proptest::test_runner::Config {
            cases: 100,
            verbose: 1,
            ..Default::default()
        };

        let number_of_nodes = 1;
        let spawn: fn() -> NodeHandle = || {
            pipe_node_handle(Path::new("../../target/debug/echo"), &[]).expect("node handle failed")
        };
        let generate_message = (0..128u8).prop_map(|i| EchoMessage::Echo {
            msg_id: 0,
            echo: format!("Please echo {}", i),
        });
        simulate(
            config,
            number_of_nodes,
            spawn,
            generate_message,
            ECHO_PROPERTY,
        )
    }
}
