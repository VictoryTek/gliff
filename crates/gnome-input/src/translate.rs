//! gliff input commands to Mutter calls, without D-Bus.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use gnome_ipc::input::{AXIS_FINISH, AXIS_SOURCE_FINGER};
use hypr_input::{Axis, InputCmd};

/// One RemoteDesktop input call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Call {
    Key { code: u32, pressed: bool },
    Button { button: u32, pressed: bool },
    Motion { x: f64, y: f64 },
    Axis { dx: f64, dy: f64, flags: u32 },
    AxisDiscrete { axis: u32, steps: i32 },
}

/// Tracks held keys and buttons and the pointer's extent.
#[derive(Debug)]
pub struct Translator {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    extent: (u32, u32),
    stopped: bool,
}

impl Translator {
    pub fn new(extent: (u32, u32)) -> Self {
        Self {
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            extent: (extent.0.max(1), extent.1.max(1)),
            stopped: false,
        }
    }

    pub fn translate(&mut self, cmd: InputCmd) -> Result<Vec<Call>> {
        if self.stopped {
            bail!("input has been stopped");
        }
        Ok(match cmd {
            InputCmd::Key { code, pressed } => {
                if pressed {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
                vec![Call::Key { code, pressed }]
            }
            InputCmd::Button { button, pressed } => {
                if pressed {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
                vec![Call::Button { button, pressed }]
            }
            InputCmd::Motion { x, y } => {
                let (w, h) = self.extent;
                vec![Call::Motion {
                    x: x.clamp(0.0, f64::from(w - 1)),
                    y: y.clamp(0.0, f64::from(h - 1)),
                }]
            }
            InputCmd::Axis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let index = match axis {
                    Axis::Vertical => 0,
                    Axis::Horizontal => 1,
                };
                if stop {
                    vec![Call::Axis {
                        dx: 0.0,
                        dy: 0.0,
                        flags: AXIS_FINISH | AXIS_SOURCE_FINGER,
                    }]
                } else if let Some(steps) = discrete {
                    vec![Call::AxisDiscrete { axis: index, steps }]
                } else {
                    let (dx, dy) = match axis {
                        Axis::Vertical => (0.0, value),
                        Axis::Horizontal => (value, 0.0),
                    };
                    vec![Call::Axis {
                        dx,
                        dy,
                        flags: AXIS_SOURCE_FINGER,
                    }]
                }
            }
            InputCmd::SetExtent { width, height } => {
                self.extent = (width.max(1), height.max(1));
                Vec::new()
            }
            InputCmd::ReleaseAll => self.release_all(),
            // Like the Hyprland input: let go of everything, then take no more.
            InputCmd::Stop => {
                self.stopped = true;
                self.release_all()
            }
            InputCmd::SetKeymap(_) => bail!(
                "keymap forwarding is not supported on GNOME yet; keys use the server's layout"
            ),
        })
    }
}

impl Translator {
    fn release_all(&mut self) -> Vec<Call> {
        let mut calls: Vec<Call> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|code| Call::Key {
                code,
                pressed: false,
            })
            .collect();
        calls.extend(
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|button| Call::Button {
                    button,
                    pressed: false,
                }),
        );
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Translator {
        Translator::new((1920, 1080))
    }

    #[test]
    fn keys_and_buttons_pass_through() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Key {
                code: 30,
                pressed: true
            })
            .unwrap(),
            [Call::Key {
                code: 30,
                pressed: true
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Button {
                button: 0x110,
                pressed: true
            })
            .unwrap(),
            [Call::Button {
                button: 0x110,
                pressed: true
            }]
        );
    }

    #[test]
    fn release_all_lets_go_of_what_is_held_once() {
        let mut t = t();
        t.translate(InputCmd::Key {
            code: 42,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Key {
            code: 30,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Key {
            code: 30,
            pressed: false,
        })
        .unwrap();
        t.translate(InputCmd::Button {
            button: 0x110,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::ReleaseAll).unwrap(),
            [
                Call::Key {
                    code: 42,
                    pressed: false
                },
                Call::Button {
                    button: 0x110,
                    pressed: false
                },
            ]
        );
        assert!(t.translate(InputCmd::ReleaseAll).unwrap().is_empty());
    }

    #[test]
    fn stop_releases_what_is_held_and_refuses_more_input() {
        let mut t = t();
        t.translate(InputCmd::Key {
            code: 42,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Button {
            button: 0x110,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::Stop).unwrap(),
            [
                Call::Key {
                    code: 42,
                    pressed: false
                },
                Call::Button {
                    button: 0x110,
                    pressed: false
                },
            ]
        );
        assert!(t
            .translate(InputCmd::Key {
                code: 30,
                pressed: true
            })
            .is_err());
        assert!(t.translate(InputCmd::Motion { x: 1.0, y: 1.0 }).is_err());
    }

    #[test]
    fn motion_is_clamped_to_the_extent_and_follows_resizes() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Motion { x: -5.0, y: 5000.0 })
                .unwrap(),
            [Call::Motion { x: 0.0, y: 1079.0 }]
        );
        t.translate(InputCmd::SetExtent {
            width: 800,
            height: 600,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::Motion { x: 900.5, y: 10.25 })
                .unwrap(),
            [Call::Motion { x: 799.0, y: 10.25 }]
        );
    }

    #[test]
    fn wheel_steps_are_discrete_on_the_right_axis() {
        let mut t = t();
        let wheel = |axis, steps| InputCmd::Axis {
            axis,
            value: 15.0,
            discrete: Some(steps),
            stop: false,
        };
        assert_eq!(
            t.translate(wheel(Axis::Vertical, 2)).unwrap(),
            [Call::AxisDiscrete { axis: 0, steps: 2 }]
        );
        assert_eq!(
            t.translate(wheel(Axis::Horizontal, -1)).unwrap(),
            [Call::AxisDiscrete { axis: 1, steps: -1 }]
        );
    }

    #[test]
    fn smooth_scrolling_uses_the_value_and_stop_finishes_it() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Vertical,
                value: 4.5,
                discrete: None,
                stop: false
            })
            .unwrap(),
            [Call::Axis {
                dx: 0.0,
                dy: 4.5,
                flags: AXIS_SOURCE_FINGER
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Horizontal,
                value: -2.0,
                discrete: None,
                stop: false
            })
            .unwrap(),
            [Call::Axis {
                dx: -2.0,
                dy: 0.0,
                flags: AXIS_SOURCE_FINGER
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Vertical,
                value: 0.0,
                discrete: None,
                stop: true
            })
            .unwrap(),
            [Call::Axis {
                dx: 0.0,
                dy: 0.0,
                flags: AXIS_FINISH | AXIS_SOURCE_FINGER
            }]
        );
    }

    #[test]
    fn a_keymap_is_refused_not_silently_ignored() {
        let err = t()
            .translate(InputCmd::SetKeymap("xkb_keymap {}".into()))
            .unwrap_err();
        assert!(err.to_string().contains("keymap"));
    }
}
