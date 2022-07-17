use crate::{ctrc::Crtc, modes::Modes, output::Output};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Resources {
    crtcs: Vec<Crtc>,
    outputs: Vec<Output>,
    modes: Vec<Modes>,
    max_width: i32,
    max_height: i32,
}

impl Resources {
    pub fn new() -> Resources {
        Resources {
            crtcs: Vec::new(),
            outputs: Vec::new(),
            modes: Vec::new(),
            max_width: 0,
            max_height: 0,
        }
    }
    pub fn handle_get_resources(
        &self,
        serial: u32,
    ) -> (u32, Vec<Crtc>, Vec<Output>, Vec<Modes>, i32, i32) {
        let Resources {
            max_height,
            max_width,
            modes,
            outputs,
            crtcs,
        } = self.clone();
        (serial, crtcs, outputs, modes, max_width, max_height)
    }
}
