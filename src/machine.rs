//! A machine determines when to inject and/or block outgoing traffic. Consists
//! of zero or more [`State`] structs.

use crate::constants::*;
use crate::event::*;
use crate::state::*;
use byteorder::ByteOrder;
use byteorder::{LittleEndian, WriteBytesExt};
use serde::Deserialize;
use serde::Serialize;
use std::error::Error;
use std::io::Write;
use std::str::FromStr;
extern crate simple_error;
use hex::{decode, encode};
use libflate::zlib::{Decoder, Encoder};
use ring::digest::{Context, SHA256};
use simple_error::{bail, map_err_with};
use std::io::Read;

/// A probabilistic state machine (Rabin automaton) consisting of zero or more
/// [`State`] that determine when to inject and/or block outgoing traffic.
#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    /// The number of bytes of padding a machine is allowed to generate as
    /// actions before other limits apply.
    pub allowed_padding_bytes: u64,
    /// The maximum fraction of padding bytes to allow as actions.
    pub max_padding_frac: f64,
    /// The number of microseconds of blocking a machine is allowed to generate
    /// as actions before other limits apply.
    pub allowed_blocked_microsec: u64,
    /// The maximum fraction of blocking (microseconds) to allow as actions.
    pub max_blocking_frac: f64,
    /// The states that make up the machine.
    pub states: Vec<State>,
    pub include_small_packets: bool,
}

impl FromStr for Machine {
    type Err = Box<dyn Error + Send + Sync>;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // hex -> zlib -> vec
        let compressed = map_err_with!(decode(s), "failed to decode hex")?;

        let mut decoder = map_err_with!(Decoder::new(&compressed[..]), "not in zlib format")?;
        let mut buf = Vec::new();
        decoder.read_to_end(&mut buf).unwrap();

        if buf.len() < 2 {
            bail!("cannot read version")
        }

        let (version, payload) = buf.split_at(2);

        match u16::from_le_bytes(version.try_into().unwrap()) {
            1 => parse_v1_machine(payload),
            v => bail!("unsupported version: {}", v),
        }
    }
}

impl Machine {
    /// Get a unique and deterministic string that represents the machine. The
    /// name is generated by hashing the serialization of the machine.
    pub fn name(&self) -> String {
        let mut context = Context::new(&SHA256);
        context.update(self.serialize().as_bytes());
        let d = context.finish();
        let s = encode(d);
        s[0..32].to_string()
    }

    /// Validates that the machine is in a valid state (machines that are
    /// mutated may get into an invalid state).
    pub fn validate(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // sane limits
        if self.max_padding_frac < 0.0 || self.max_padding_frac > 1.0 {
            bail!(
                "max_padding_frac has to be [0.0, 1.0], got {}",
                self.max_padding_frac
            )
        }
        if self.max_blocking_frac < 0.0 || self.max_blocking_frac > 1.0 {
            bail!(
                "max_blocking_frac has to be [0.0, 1.0], got {}",
                self.max_blocking_frac
            )
        }

        // sane number of states
        if self.states.is_empty() {
            bail!("a machine must have at least one state")
        }
        if self.states.len() > STATEMAX {
            bail!(
                "too many states, max is {}, found {}",
                STATEMAX,
                self.states.len()
            )
        }

        // check each state
        for state in &self.states {
            // validate transitions
            for next in &state.next_state {
                if next.1.len() != self.states.len() + 2 {
                    bail!(
                        "found too small next_state vector, expected {}, got {}",
                        self.states.len() + 2,
                        next.1.len()
                    )
                }

                let mut p_total = 0.0;
                for p in next.1 {
                    if !(&0.0..=&1.0).contains(&p) {
                        bail!("found probability {}, has to be [0.0, 1.0]", &p)
                    }
                    p_total += p;
                }

                // we are (0.0, 1.0] here, because:
                // - if pTotal <= 0.0, then we shouldn't have an entry in NextState
                // - pTotal < 1.0 is OK, to support a "nop" transition (self
                // transition has implications in the framework, i.e., involving
                // limits on padding sent in he state)
                if p_total <= 0.0 || p_total >= 1.0005 {
                    // 1.0005 due to rounding
                    bail!(
                        "found invalid total probability vector {}, must be (0.0, 1.0]",
                        p_total
                    )
                }
            }

            // validate distribution parameters
            state.action.validate()?;
            state.limit.validate()?;
            state.timeout.validate()?;
        }

        Ok(())
    }

    /// Serialize the machine into a byte vector.
    pub fn serialize(&self) -> String {
        let mut wtr = vec![];

        wtr.write_u16::<LittleEndian>(VERSION as u16).unwrap();
        wtr.write_u64::<LittleEndian>(self.allowed_padding_bytes)
            .unwrap();
        wtr.write_f64::<LittleEndian>(self.max_padding_frac)
            .unwrap();
        wtr.write_u64::<LittleEndian>(self.allowed_blocked_microsec)
            .unwrap();
        wtr.write_f64::<LittleEndian>(self.max_blocking_frac)
            .unwrap();

        if self.include_small_packets {
            wtr.write_u8(1).unwrap();
        } else {
            wtr.write_u8(0).unwrap();
        }

        let num_states = self.states.len();
        wtr.write_u16::<LittleEndian>(num_states as u16).unwrap();

        for i in 0..self.states.len() {
            wtr.write_all(&self.states[i].serialize(num_states))
                .unwrap();
        }

        let mut encoder = Encoder::new(Vec::new()).unwrap();
        encoder.write_all(&wtr).unwrap();
        let compressed = encoder.finish().into_result().unwrap();

        // return hex encoded string
        encode(compressed)
    }
}

fn parse_v1_machine(buf: &[u8]) -> Result<Machine, Box<dyn Error + Send + Sync>> {
    // note that we already read 2 bytes of version in fn parse_machine()
    if buf.len() < 4 * 8 + 1 + 2 {
        bail!("not enough data for version 1 machine")
    }

    let mut r: usize = 0;
    // 4 8-byte values
    let allowed_padding_bytes = LittleEndian::read_u64(&buf[r..r + 8]);
    r += 8;
    let max_padding_frac = LittleEndian::read_f64(&buf[r..r + 8]);
    r += 8;
    let allowed_blocked_microsec = LittleEndian::read_u64(&buf[r..r + 8]);
    r += 8;
    let max_blocking_frac = LittleEndian::read_f64(&buf[r..r + 8]);
    r += 8;

    // 1-byte flag
    let include_small_packets = buf[r] == 1;
    r += 1;

    // 2-byte num of states
    let num_states: usize = LittleEndian::read_u16(&buf[r..r + 2]) as usize;
    r += 2;

    // each state has 3 distributions + 4 flags + next_state matrix
    let expected_state_len: usize =
        3 * SERIALIZEDDISTSIZE + 4 + (num_states + 2) * 8 * Event::iterator().len();
    if buf[r..].len() != expected_state_len * num_states {
        bail!(format!(
            "expected {} bytes for {} states, but got {} bytes",
            expected_state_len * num_states,
            num_states,
            buf[r..].len()
        ))
    }

    let mut states = vec![];
    for _ in 0..num_states {
        let s = parse_state(buf[r..r + expected_state_len].to_vec(), num_states).unwrap();
        r += expected_state_len;
        states.push(s);
    }

    let m = Machine {
        allowed_padding_bytes,
        max_padding_frac,
        allowed_blocked_microsec,
        max_blocking_frac,
        include_small_packets,
        states,
    };
    m.validate()?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use crate::dist::*;
    use crate::machine::*;
    use std::collections::HashMap;

    #[test]
    fn basic_serialization() {
        // plan: manually create a machine, serialize it, parse it, and then compare
        let num_states = 2;

        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e0: HashMap<usize, f64> = HashMap::new();
        e0.insert(0, 0.4);
        e0.insert(1, 0.6);
        let mut e1: HashMap<usize, f64> = HashMap::new();
        e1.insert(1, 1.0);
        t.insert(Event::PaddingRecv, e0);
        t.insert(Event::LimitReached, e1);
        let mut s0 = State::new(t, num_states);
        s0.timeout = Dist {
            dist: DistType::Poisson,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s0.limit = Dist {
            dist: DistType::Pareto,
            param1: 9.0,
            param2: 1.2,
            start: 3.4,
            max: 5.6,
        };
        s0.action = Dist {
            dist: DistType::Geometric,
            param1: 0.8,
            param2: 9.0,
            start: 1.2,
            max: 3.4,
        };

        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e0: HashMap<usize, f64> = HashMap::new();
        e0.insert(0, 0.2);
        e0.insert(1, 0.8);
        let mut e1: HashMap<usize, f64> = HashMap::new();
        e1.insert(0, 1.0);
        t.insert(Event::NonPaddingRecv, e0);
        t.insert(Event::PaddingSent, e1);
        let mut s1 = State::new(t, num_states);
        s1.timeout = Dist {
            dist: DistType::Uniform,
            param1: 0.1,
            param2: 1.2,
            start: 3.4,
            max: 5.6,
        };
        s1.limit = Dist {
            dist: DistType::Weibull,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s1.action = Dist {
            dist: DistType::Beta,
            param1: 5.6,
            param2: 7.8,
            start: 9.0,
            max: 1.2,
        };
        s1.action_is_block = true;

        let m = Machine {
            allowed_padding_bytes: 1000,
            max_padding_frac: 0.123,
            allowed_blocked_microsec: 2000,
            max_blocking_frac: 0.456,
            states: vec![s0, s1],
            include_small_packets: true,
        };

        // serialize, parse, eq
        let s = m.serialize();
        let m_parsed = Machine::from_str(&s).unwrap();
        assert_eq!(m, m_parsed);
    }

    #[test]
    fn parse_v1_machine_nop() {
        // attempt to parse an empty no-op machine (does nothing)
        let s = "789cedca2101000000c230e85f1a8387009f9e351d051503ca0003".to_string();
        let m = Machine::from_str(&s).unwrap();

        assert_eq!(m.allowed_blocked_microsec, 0);
        assert_eq!(m.allowed_padding_bytes, 0);
        assert_eq!(m.max_blocking_frac, 0.0);
        assert_eq!(m.max_padding_frac, 0.0);
        assert_eq!(m.include_small_packets, false);

        assert_eq!(m.states.len(), 1);
        assert_eq!(m.states[0].replace, false);
        assert_eq!(m.states[0].limit_includes_nonpadding, false);
        assert_eq!(m.states[0].action_is_block, false);
        assert_eq!(m.states[0].action.dist, DistType::None);
        assert_eq!(m.states[0].action.param1, 0.0);
        assert_eq!(m.states[0].action.param2, 0.0);
        assert_eq!(m.states[0].action.max, 0.0);
        assert_eq!(m.states[0].action.start, 0.0);
        assert_eq!(m.states[0].limit.dist, DistType::None);
        assert_eq!(m.states[0].limit.param1, 0.0);
        assert_eq!(m.states[0].limit.param2, 0.0);
        assert_eq!(m.states[0].limit.max, 0.0);
        assert_eq!(m.states[0].limit.start, 0.0);
        assert_eq!(m.states[0].timeout.dist, DistType::None);
        assert_eq!(m.states[0].timeout.param1, 0.0);
        assert_eq!(m.states[0].timeout.param2, 0.0);
        assert_eq!(m.states[0].timeout.max, 0.0);
        assert_eq!(m.states[0].timeout.start, 0.0);

        assert_eq!(m.states[0].next_state.len(), 0);
    }

    #[test]
    fn parse_v1_machine_padding() {
        // make a 1-state padding machine, serialize, and compare
        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e: HashMap<usize, f64> = HashMap::new();
        e.insert(0, 1.0);
        t.insert(Event::PaddingSent, e);
        let mut s0 = State::new(t, 1);
        s0.timeout = Dist {
            dist: DistType::Uniform,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s0.action = Dist {
            dist: DistType::Poisson,
            param1: 0.5,
            param2: 0.0,
            start: 1.2,
            max: 3.4,
        };
        let m = Machine {
            allowed_padding_bytes: 1000,
            max_padding_frac: 0.123,
            allowed_blocked_microsec: 0,
            max_blocking_frac: 0.0,
            states: vec![s0],
            include_small_packets: false,
        };
        let s = m.serialize();
        println!("{}", s);
        let m_parsed = Machine::from_str(&s).unwrap();
        assert_eq!(m, m_parsed);

        // add hardcoded assert
        let hardcoded = "789cbdcebb0d80201006e0bb5858d8db3a8403c034c6dada25dcc40d5cc59286848405f8b9828450000d5f71b947ee724c6622f15ee763ef4f21cd31cd88d19f86bbf00a01168d5605173b8758350ad81a6ef472e9df5102a4ac13d3".to_string();
        let m_hardcoded = Machine::from_str(&hardcoded).unwrap();
        assert_eq!(m, m_hardcoded);
    }

    #[test]
    fn parse_v1_machine_blocking() {
        // make a 1-state blocking machine, serialize, and compare
        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e: HashMap<usize, f64> = HashMap::new();
        e.insert(0, 1.0);
        t.insert(Event::BlockingEnd, e);
        let mut s0 = State::new(t, 1);
        s0.timeout = Dist {
            dist: DistType::Pareto,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s0.action = Dist {
            dist: DistType::Geometric,
            param1: 0.3,
            param2: 0.7,
            start: 3.4,
            max: 7.9,
        };
        s0.action_is_block = true;
        let m = Machine {
            allowed_padding_bytes: 0,
            max_padding_frac: 0.0,
            allowed_blocked_microsec: 100000,
            max_blocking_frac: 0.9999,
            states: vec![s0],
            include_small_packets: true,
        };
        let s = m.serialize();
        println!("{}", s);
        let m_parsed = Machine::from_str(&s).unwrap();
        assert_eq!(m, m_parsed);

        // add hardcoded assert
        let hardcoded = "789cc5cda11180300c05d04480c123e9061806482493300a3b80c231103b7038b86300f8b45c454d45459fc85d72c90f536819ddac598fbe7d4e61a6823a6b93c1da050d543a4f1fa3d88f28ff8cdbdf22086a450346dddb9c2e4149f20205f11a22".to_string();
        let m_hardcoded = Machine::from_str(&hardcoded).unwrap();
        assert_eq!(m, m_hardcoded);
    }

    #[test]
    fn parse_v1_machine_mixed() {
        // make a 2-state mixed machine, serialize, and compare
        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e: HashMap<usize, f64> = HashMap::new();
        e.insert(1, 1.0);
        t.insert(Event::BlockingEnd, e);
        let mut s0 = State::new(t, 2);
        s0.timeout = Dist {
            dist: DistType::Pareto,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s0.action = Dist {
            dist: DistType::Geometric,
            param1: 0.3,
            param2: 0.7,
            start: 3.4,
            max: 7.9,
        };
        s0.action_is_block = true;
        let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
        let mut e: HashMap<usize, f64> = HashMap::new();
        e.insert(0, 1.0);
        t.insert(Event::PaddingSent, e);
        let mut s1 = State::new(t, 2);
        s1.timeout = Dist {
            dist: DistType::Uniform,
            param1: 1.2,
            param2: 3.4,
            start: 5.6,
            max: 7.8,
        };
        s1.action = Dist {
            dist: DistType::Poisson,
            param1: 0.5,
            param2: 0.0,
            start: 1.2,
            max: 3.4,
        };
        let m = Machine {
            allowed_padding_bytes: 0,
            max_padding_frac: 0.0,
            allowed_blocked_microsec: 100000,
            max_blocking_frac: 0.9999,
            states: vec![s0, s1],
            include_small_packets: true,
        };
        let s = m.serialize();
        println!("{}", s);
        let m_parsed = Machine::from_str(&s).unwrap();
        assert_eq!(m, m_parsed);

        // add hardcoded assert
        let hardcoded = "789cd5d0b10980301005d044500b7b4bb3818d03e44a27711477d0cace815cc04aec141c407f1249408b3441f0410239ee2ef0397b1a5a532bc6b52ecf4df288c5acd226d9688bc40332ea3b4510fa3d927bc76167b10872c20304996fff6497b8824a7194d96e4634e05243c983bf661033b8a4d1f491f00985720190af2886".to_string();
        let m_hardcoded = Machine::from_str(&hardcoded).unwrap();
        assert_eq!(m, m_hardcoded);
    }

    #[test]
    fn parse_v1_machine_100_states() {
        // make a machine with 100 states, serialize, and compare
        let num_states = 100;
        let mut states: Vec<State> = vec![];
        for i in 0..num_states {
            let mut t: HashMap<Event, HashMap<usize, f64>> = HashMap::new();
            let mut e: HashMap<usize, f64> = HashMap::new();
            e.insert(i, 1.0);
            t.insert(Event::PaddingSent, e);
            let mut s = State::new(t, num_states);
            s.timeout = Dist {
                dist: DistType::Uniform,
                param1: 1.2,
                param2: 3.4,
                start: 5.6,
                max: 7.8,
            };
            s.action = Dist {
                dist: DistType::Poisson,
                param1: 0.5,
                param2: 0.0,
                start: 1.2,
                max: 3.4,
            };
            states.push(s);
        }
        let m = Machine {
            allowed_padding_bytes: 0,
            max_padding_frac: 0.0,
            allowed_blocked_microsec: 100000,
            max_blocking_frac: 0.9999,
            states,
            include_small_packets: true,
        };
        let s = m.serialize();
        println!("{}", s);
        let m_parsed = Machine::from_str(&s).unwrap();
        assert_eq!(m, m_parsed);

        let hardcoded = "789cedd93b8e54311040d1e988809c107640c2027a421682580aeb20635b846420b10078dda381999efebcafed2a9f1358aa92ecd8d2dddd9dfafae561f7f6db8f8feffffcdcef3eddbd1ac683effbe138fa70f47b3f1c83d7f7c3ea86dd8b3b9f8fdedc0fc3e0dd8837000028e3d7bf7f1f000000109fd20300d027c50700000032507a0000faa6f800000040644a0f0000078a0f00000044a4f40000f094e20300000091283d00009ca3f800000040044a0f0000d7283e000000d032a507008031141f0000006891d20300c0148a0f000000b444e90100600ec5070000005aa0f40000b084e20300000035293d0000ac41f1010000801a941e0000d6a4f800000040494a0f00005b507c000000a004a50700802d293e000000b025a507008012141f000000d882d2030040498a0f000000ac49e90100a006c507000000d6a0f400005093e2030000004b283d0000b440f10100008039941e00005aa2f8000000c0144a0f00002d527c000000600ca507008096293e000000708dd2030040048a0f0000009ca3f400001089e2030000004f293d000044a4f8000000c081d2030040648a0f0000007d537a0000c840f1010000a04f4a0f000099283e000000f445e901002023c5070000803e283d000064a6f8000000909bd20300400f141f00000072527a0000e889e2030000402e4a0f00003d527c000000c841e90100a0678a0f000000b1293d0000a0f80000001095d2030000ff293e000000c4a2f40000c04b8a0f00000031283d00007099e203000040db941e0000b84df1010000a04d4a0f00008ca7f8000000d016a5070000a6537c0000006883d2030000f3293e000000d4a5f40000c0728a0f00000075283d0000b01ec507000080b2941e0000589fe203000040194a0f00006c47f1010000605b4a0f00006c4ff1010000601b4a0f000094a3f8000000b02ea5070000ca537c0000005887d2030000f5283e0000002ca3f40000407d8a0f000000f3283d0000d00ec50700008069941e0000688fe2030000c0384a0f0000b44bf1010000e03aa5070000daa7f8000000709ed203000071283e0000003ca7f40000403c8a0f0000000f941e0000884bf1010000e89dd2030000f1293e000000bd527a0000200fc5070000a0374a0f0000e4a3f8000000f442e9010080bc141f000080ec941e0000c84ff1010000c84ae90100807e283e000000d9283d0000d01fc5070000200ba5070000faa5f800000044a7f40000008a0f000040544a0f0000f048f10100008846e90100004e293e00000051283d0000c0258a0f000040eb941e0000e016c5070000a0554a0f00003096e2030000d01aa5070000984af10100006885d2030000cca5f8000000d4a6f40000004b293e000000b5283d0000c05a141f000080d2941e0000606d8a0f000040294a0f0000b015c5070000606b4a0f0000b035c5070000602b4a0f0000508ae2030000b036a5070000284df1010000588bd2030000d4a2f80000002ca5f4000000b5293e00000073293d0000402b141f000080a9941e0000a0358a0f0000c0584a0f0000d02ac5070000e016a5070000689de20300007089d203000044a1f80000009c527a00008068141f000080474a0f00001095e2030000a0f4000000d1293e000040bf941e0000200bc5070000e88fd203000064a3f8000000fd507a000080ac141f0000203fa5070000c84ef1010000f2527a0000805e283e0000403e4a0f0000d01bc5070000c843e90100007aa5f8000000f1293d000040ef141f000020aebf891aa4d5".to_string();
        let m_hardcoded = Machine::from_str(&hardcoded).unwrap();
        assert_eq!(m, m_hardcoded);
    }
}
