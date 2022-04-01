use std::sync::{Arc, Mutex};

use crate::{
    ctrl_surf::{
        self,
        event::{self, *},
        Error, Msg,
    },
    midi,
};

mod connection {
    pub const MACKIE_ID: [u8; 3] = [0x00, 0x00, 0x66];

    pub const XTOUCH_ID: u8 = 0x14;
    //pub const XTOUCH_EXT_ID: u8 = 0x15;

    pub const LOGIC_CONTROL_ID: u8 = 0x10;
    pub const LOGIC_CONTROL_EXT_ID: u8 = 0x11;

    pub const QUERY_DEVICE: u8 = 0x00;
    pub const QUERY_HOST: u8 = 0x01;
    pub const HOST_REPLY: u8 = 0x02;
    pub const DEVICE_OK: u8 = 0x03;
    pub const DEVICE_ERR: u8 = 0x04;

    // For some reasons, sending these two doesn't work with XTouch-One
    //pub const RESET_FADERS: u8 = 0x61;
    //pub const RESET_LEDS: u8 = 0x62;

    // TODO?
    //pub const GO_OFFLINE: u8 = 0x0f;
}

mod button {
    use crate::midi::Tag;
    pub const TAG: Tag = Tag::from(0x90);

    pub const PRESSED: u8 = 127;
    pub const RELEASED: u8 = 0;
    pub const ON: u8 = PRESSED;
    pub const OFF: u8 = RELEASED;

    pub const PREVIOUS: u8 = 91;
    pub const NEXT: u8 = 92;
    pub const STOP: u8 = 93;
    pub const PLAY: u8 = 94;
    pub const FADER_TOUCHED: u8 = 104;
}

mod display_7_seg {
    use crate::midi::Tag;
    pub const TAG: Tag = Tag::from(0xb0);

    pub const TIME_LEFT_DIGIT: u8 = 0x49;
}

mod fader {
    use crate::midi::Tag;
    pub const TAG: Tag = Tag::from(0xe0);

    pub const TOUCH_THRSD: u8 = 64;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Connecting(ConnectionStatus),
    Connected,
    Disconnected,
    Playing,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ConnectionStatus {
    DeviceQueried,
    ChallengeReplied,
}

#[derive(Clone, Copy, Debug)]
enum FaderState {
    Released,
    Touched { last_volume: Option<f64> },
}

#[derive(Debug)]
pub struct Mackie {
    last_tc: TimecodeBreakDown,
    chan: midi::Channel,
    state: State,
    fader_state: FaderState,

    // FIXME maintain a set of the device ids received
    // so that we can reset / disconnect all of them..
    device_id: Option<u8>,
}

impl Default for Mackie {
    fn default() -> Self {
        Self {
            last_tc: TimecodeBreakDown::default(),
            chan: midi::Channel::default(),
            state: State::Disconnected,
            fader_state: FaderState::Released,
            device_id: None,
        }
    }
}

impl crate::ctrl_surf::ControlSurface for Mackie {
    fn start_identification(&mut self) -> Vec<Msg> {
        use connection::*;

        log::debug!("Starting device identification");

        *self = Mackie {
            state: State::Connecting(ConnectionStatus::DeviceQueried),
            ..Default::default()
        };

        // need a way to specify which device we want to query.
        midi::Msg::new_sysex(&Self::payload_for(XTOUCH_ID, QUERY_DEVICE))
            .to_device()
            .into()
    }

    fn msg_from_device(&mut self, msg: crate::midi::Msg) -> Vec<Msg> {
        let buf = msg.inner();

        if let Some(&tag_chan) = buf.first() {
            self.chan = midi::Channel::from(tag_chan);

            match midi::Tag::from_tag_chan(tag_chan) {
                button::TAG => {
                    if let Some(id_value) = buf.get(1..=2) {
                        use button::*;
                        use Transport::*;

                        match id_value {
                            [PREVIOUS, PRESSED] => return Previous.to_app().into(),
                            [NEXT, PRESSED] => return Next.to_app().into(),
                            [STOP, PRESSED] => return Stop.to_app().into(),
                            [PLAY, PRESSED] => return PlayPause.to_app().into(),
                            [FADER_TOUCHED, value] => return self.device_fader_touch(*value),
                            _ => (),
                        }
                    }
                }
                fader::TAG => {
                    if let Some(value) = buf.get(1..=2) {
                        return self.device_fader_moved(value);
                    }
                }
                midi::sysex::TAG => return self.device_sysex(msg),
                _ => (),
            }
        }

        Msg::none()
    }

    fn event_to_device(&mut self, event: Feedback) -> Vec<Msg> {
        if !self.is_connected() {
            log::debug!("Ignoring event: Control surface not connected.");
            return Msg::none();
        }

        use Feedback::*;
        match event {
            Transport(event) => {
                use event::Transport::*;
                match event {
                    Play => return self.app_play(),
                    Pause => return self.app_pause(),
                    Stop => {
                        // FIXME go offline
                        return self.reset();
                    }
                    _ => (),
                }
            }
            Mixer(mixer) => {
                use event::Mixer::*;
                match mixer {
                    Volume(vol) => return self.app_volume(vol),
                    Mute => (),
                }
            }
            NewApp(app) => {
                log::debug!("New application {app}. Reseting and requesting data");
                let mut msg_list = self.reset();
                msg_list.push(CtrlSurfEvent::DataRequest.into());

                return msg_list;
            }
            Data(data) => {
                use event::Data::*;
                match data {
                    Timecode(tc) => return self.app_timecode(tc),
                    AppName(player) => {
                        log::debug!("got {}", player);
                        // FIXME send to player name to device
                    }
                    Track(_) => (),
                }
            }
        }

        Msg::none()
    }

    fn is_connected(&self) -> bool {
        !matches!(self.state, State::Connecting(_) | State::Disconnected)
    }

    fn reset(&mut self) -> Vec<Msg> {
        use button::*;
        use display_7_seg::*;
        use State::*;

        let mut list = Vec::new();

        let tag_chan = button::TAG | self.chan;
        list.push([tag_chan, PREVIOUS, OFF].into());
        list.push([tag_chan, NEXT, OFF].into());
        list.push([tag_chan, STOP, OFF].into());
        list.push([tag_chan, PLAY, OFF].into());

        for idx in 0..10 {
            list.push([display_7_seg::TAG.into(), TIME_LEFT_DIGIT - idx as u8, b' '].into());
        }

        let state = match self.state {
            Connected | Playing | Stopped => Connected,
            other => other,
        };

        *self = Self {
            state,
            ..Default::default()
        };

        list
    }
}

/// Device events.
impl Mackie {
    fn build_fader_msg(&self, vol: f64) -> Msg {
        let two_bytes = midi::normalized_f64::to_be(vol).unwrap();
        [fader::TAG | self.chan, two_bytes[0], two_bytes[1]].into()
    }

    fn device_fader_touch(&mut self, value: u8) -> Vec<Msg> {
        use FaderState::*;
        use Mixer::*;

        let is_touched = value > fader::TOUCH_THRSD;
        match self.fader_state {
            Released if is_touched => {
                self.fader_state = Touched { last_volume: None };
            }
            Touched { last_volume } if !is_touched => {
                self.fader_state = Released;
                if let Some(vol) = last_volume {
                    return vec![Volume(vol).to_app(), self.build_fader_msg(vol)];
                }
            }
            _ => (),
        }

        Msg::none()
    }

    fn device_fader_moved(&mut self, buf: &[u8]) -> Vec<Msg> {
        use FaderState::*;
        use Mixer::*;

        let vol = match midi::normalized_f64::from_be(buf) {
            Ok(value) => value,
            Err(err) => {
                log::error!("Fader moved value: {err}");
                return Msg::none();
            }
        };

        match &mut self.fader_state {
            Touched { last_volume } => {
                *last_volume = Some(vol);
                Volume(vol).to_app().into()
            }
            Released => {
                // FIXME is this a problem or even possible?
                Volume(vol).to_app().into()
            }
        }
    }
}

/// App events.
impl Mackie {
    fn app_play(&mut self) -> Vec<Msg> {
        use button::*;
        use State::*;

        let mut list = Vec::new();
        let tag_chan = button::TAG | self.chan;

        match self.state {
            Connected | Stopped => {
                self.state = Playing;
                list.push([tag_chan, STOP, OFF].into());
            }
            Playing => (),
            Connecting(_) | Disconnected => unreachable!(),
        }

        list.push([tag_chan, PLAY, ON].into());

        list
    }

    fn app_pause(&mut self) -> Vec<Msg> {
        use button::*;
        use State::*;

        let mut list = Vec::new();
        let tag_chan = button::TAG | self.chan;

        match self.state {
            Connected | Playing => {
                self.state = Stopped;
                list.push([tag_chan, PLAY, OFF].into());
            }
            Stopped => (),
            Connecting(_) | Disconnected => unreachable!(),
        }

        list.push([tag_chan, STOP, ON].into());

        list
    }

    fn app_volume(&mut self, vol: f64) -> Vec<Msg> {
        use FaderState::*;

        match &mut self.fader_state {
            Released => self.build_fader_msg(vol).into(),
            Touched { last_volume } => {
                // user touches fader => don't move it before it's released.
                *last_volume = Some(vol);

                Msg::none()
            }
        }
    }

    fn app_timecode(&mut self, tc: ctrl_surf::Timecode) -> Vec<Msg> {
        use display_7_seg::*;

        let mut list = Vec::new();
        let tc = TimecodeBreakDown::from(tc);

        for (idx, (&last_digit, digit)) in self.last_tc.0.iter().zip(tc.0).enumerate() {
            if last_digit != digit {
                list.push([TAG.into(), TIME_LEFT_DIGIT - idx as u8, digit].into());
            }
        }

        self.last_tc = tc;

        list
    }
}

/// Device handshake.
impl Mackie {
    fn device_sysex(&mut self, msg: midi::Msg) -> Vec<Msg> {
        self.device_connection(msg)
            .unwrap_or_else(|err| Msg::from_connection_result(Err(err)).into())
    }

    fn device_connection(&mut self, msg: midi::Msg) -> Result<Vec<Msg>, Error> {
        use crate::bytes::Displayable;
        use connection::*;
        use Error::*;

        let payload = msg.parse_sysex()?;

        // Check header
        if payload.len() < 5 {
            return Err(UnexpectedDeviceMsg(msg.display().to_owned()));
        }

        if payload[0..3] != MACKIE_ID {
            return Err(ManufacturerMismatch {
                expected: Displayable::from(MACKIE_ID.as_slice()).to_owned(),
                found: Displayable::from(&payload[0..3]).to_owned(),
            });
        }

        let device_id = payload[3];

        let msg_list = match (payload[4], payload.get(5..)) {
            (QUERY_HOST, Some(serial_challenge)) => self
                .device_query_host(device_id, serial_challenge)
                .map_err(|_| UnexpectedDeviceMsg(msg.display().to_owned()))?,
            (DEVICE_OK, Some(_serial)) => self.device_connected(device_id),
            (DEVICE_ERR, Some(_serial)) => {
                self.state = State::Disconnected;
                log::error!("Device connection failed");
                return Err(ConnectionError);
            }
            (QUERY_DEVICE, _) => {
                self.state = State::Disconnected;
                log::error!("Device sent QUERY DEVICE");
                return Err(UnexpectedDeviceMsg(msg.display().to_owned()));
            }
            (msg_id, _) => {
                self.state = State::Disconnected;
                log::error!("Device sent unexpected msg {msg_id:02x}");
                return Err(UnexpectedDeviceMsg(msg.display().to_owned()));
            }
        };

        Ok(msg_list)
    }

    fn device_query_host(
        &mut self,
        device_id: u8,
        serial_challenge: &[u8],
    ) -> Result<Vec<Msg>, ()> {
        use connection::*;

        let (ser, chlg) = serial_challenge
            .get(..7)
            .zip(serial_challenge.get(7..11))
            .ok_or_else(|| {
                self.state = State::Disconnected;
                log::error!("Device QUERY HOST: invalid serial / challenge");
            })?;

        let msg_list = if device_id == LOGIC_CONTROL_ID || device_id == LOGIC_CONTROL_EXT_ID {
            let mut resp = [0u8; 5 + 7 + 4];

            Self::prepare_payload(&mut resp, device_id, HOST_REPLY);
            resp[5..12].copy_from_slice(ser);
            resp[12] = 0x7F & (chlg[0] + (chlg[1] ^ 0x0a) - chlg[3]);
            resp[13] = 0x7F & ((chlg[2] >> 4) ^ (chlg[0] + chlg[3]));
            resp[14] = 0x7F & ((chlg[3] - (chlg[2] << 2)) ^ (chlg[0] | chlg[1]));
            resp[15] = 0x7F & (chlg[1] - chlg[2] + (0xf0 ^ (chlg[3] << 4)));

            self.state = State::Connecting(ConnectionStatus::ChallengeReplied);
            log::debug!("Device QUERY HOST challenge replied");

            vec![
                midi::Msg::new_sysex(&resp).to_device(),
                Msg::connetion_in_progress(),
            ]
        } else {
            // No need for a challenge reply
            self.device_connected(device_id)
        };

        Ok(msg_list)
    }

    fn device_connected(&mut self, device_id: u8) -> Vec<Msg> {
        self.device_id = Some(device_id);
        self.state = State::Connected;
        log::debug!("Device connected");

        vec![
            Msg::from_connection_result(Ok(())),
            CtrlSurfEvent::DataRequest.to_app(),
        ]
    }

    fn payload_for(device_id: u8, req_id: u8) -> [u8; 5] {
        let mut payload = [0u8; 5];
        Self::prepare_payload(&mut payload, device_id, req_id);

        payload
    }

    fn prepare_payload(payload: &mut [u8], device_id: u8, req_id: u8) {
        payload[..=2].copy_from_slice(&connection::MACKIE_ID);
        payload[3] = device_id;
        payload[4] = req_id;
    }
}

impl crate::ctrl_surf::Buildable for Mackie {
    const NAME: &'static str = "Mackie";

    fn build() -> crate::ctrl_surf::ControlSurfaceArc {
        Arc::new(Mutex::new(Self::default()))
    }
}

#[derive(Debug)]
struct TimecodeBreakDown([u8; 10]);

impl Default for TimecodeBreakDown {
    fn default() -> Self {
        Self([b' '; 10])
    }
}

impl From<ctrl_surf::Timecode> for TimecodeBreakDown {
    fn from(tc: ctrl_surf::Timecode) -> Self {
        use std::io::Write;

        let printable = format!("{:>13.3}", tc);
        let bytes = printable.as_bytes();

        let mut this = Self::default();

        let mut cur = std::io::Cursor::new(this.0.as_mut_slice());
        cur.write_all(&bytes[..=2]).unwrap();
        cur.write_all(&bytes[4..=5]).unwrap();
        cur.write_all(&bytes[7..=8]).unwrap();
        cur.write_all(&bytes[10..=12]).unwrap();

        this
    }
}