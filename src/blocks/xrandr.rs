use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

use crossbeam_channel::Sender;
use regex::RegexSet;
use serde_derive::Deserialize;

use crate::blocks::{Block, ConfigBlock, Update};
use crate::config::{LogicalDirection, SharedConfig};
use crate::de::deserialize_duration;
use crate::errors::*;
use crate::input::{I3BarEvent, MouseButton};
use crate::scheduler::Task;
use crate::subprocess::spawn_child_async;
use crate::util::FormatTemplate;
use crate::widgets::text::TextWidget;
use crate::widgets::I3BarWidget;

struct Monitor {
    name: String,
    brightness: u32,
    resolution: String,
}

impl Monitor {
    fn new(name: &str, brightness: u32, resolution: &str) -> Self {
        Monitor {
            name: String::from(name),
            brightness,
            resolution: String::from(resolution),
        }
    }

    fn set_brightness(&mut self, step: i32) {
        spawn_child_async(
            "xrandr",
            &[
                "--output",
                &self.name,
                "--brightness",
                &format!("{}", (self.brightness as i32 + step) as f32 / 100.0),
            ],
        )
        .expect("Failed to set xrandr output.");
        self.brightness = (self.brightness as i32 + step) as u32;
    }
}

pub struct Xrandr {
    id: usize,
    text: TextWidget,
    update_interval: Duration,
    monitors: Vec<Monitor>,
    icons: bool,
    resolution: bool,
    step_width: u32,
    current_idx: usize,
    shared_config: SharedConfig,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct XrandrConfig {
    /// Update interval in seconds
    #[serde(
        default = "XrandrConfig::default_interval",
        deserialize_with = "deserialize_duration"
    )]
    pub interval: Duration,

    /// Show icons for brightness and resolution (needs awesome fonts support)
    #[serde(default = "XrandrConfig::default_icons")]
    pub icons: bool,

    /// Shows the screens resolution
    #[serde(default = "XrandrConfig::default_resolution")]
    pub resolution: bool,

    /// The steps brightness is in/decreased for the selected screen (When greater than 50 it gets limited to 50)
    #[serde(default = "XrandrConfig::default_step_width")]
    pub step_width: u32,
}

impl XrandrConfig {
    fn default_interval() -> Duration {
        Duration::from_secs(5)
    }

    fn default_icons() -> bool {
        true
    }

    fn default_resolution() -> bool {
        false
    }

    fn default_step_width() -> u32 {
        5
    }
}

macro_rules! unwrap_or_continue {
    ($e: expr) => {
        match $e {
            Some(e) => e,
            None => continue,
        }
    };
}

impl Xrandr {
    fn get_active_monitors() -> Result<Option<Vec<String>>> {
        let active_monitors_cli = String::from_utf8(
            Command::new("xrandr")
                .args(&["--listactivemonitors"])
                .output()
                .block_error("xrandr", "couldn't collect active xrandr monitors")?
                .stdout,
        )
        .block_error("xrandr", "couldn't parse xrandr monitor list")?;

        let monitors: Vec<&str> = active_monitors_cli.split('\n').collect();
        let mut active_monitors: Vec<String> = Vec::new();
        for monitor in monitors {
            if let Some((name, _)) = monitor
                .split_whitespace()
                .collect::<Vec<&str>>()
                .split_last()
            {
                active_monitors.push(String::from(*name));
            }
        }
        if !active_monitors.is_empty() {
            Ok(Some(active_monitors))
        } else {
            Ok(None)
        }
    }

    fn get_monitor_metrics(monitor_names: &[String]) -> Result<Option<Vec<Monitor>>> {
        let mut monitor_metrics: Vec<Monitor> = Vec::new();
        let monitor_info_cli = String::from_utf8(
            Command::new("xrandr")
                .args(&["--verbose"])
                .output()
                .block_error("xrandr", "couldn't collect xrandr monitor info")?
                .stdout,
        )
        .block_error("xrandr", "couldn't parse xrandr monitor info")?;

        let regex_set = RegexSet::new(
            monitor_names
                .iter()
                .map(|x| format!("{} connected", x))
                .chain(std::iter::once("Brightness:".to_string())),
        )
        .block_error("xrandr", "invalid monitor name")?;

        let monitor_infos: Vec<&str> = monitor_info_cli
            .split('\n')
            .filter(|l| regex_set.is_match(l))
            .collect();
        for chunk in monitor_infos.chunks_exact(2) {
            let mut brightness = 0;
            let mut display: &str = "";
            let mi_line = unwrap_or_continue!(chunk.get(0));
            let b_line = unwrap_or_continue!(chunk.get(1));
            let mi_line_args: Vec<&str> = mi_line.split_whitespace().collect();
            if let Some(name) = mi_line_args.get(0) {
                display = name.trim();
                if let Some(brightness_raw) = b_line.split(':').collect::<Vec<&str>>().get(1) {
                    brightness = (f32::from_str(brightness_raw.trim())
                        .block_error("xrandr", "unable to parse brightness")?
                        * 100.0)
                        .floor() as u32;
                }
            }
            if let Some(mut res) = mi_line_args.get(2) {
                if res.find('+').is_none() {
                    res = unwrap_or_continue!(mi_line_args.get(3));
                }
                if let Some(resolution) = res.split('+').collect::<Vec<&str>>().get(0) {
                    monitor_metrics.push(Monitor::new(display, brightness, resolution.trim()));
                }
            }
        }
        if !monitor_metrics.is_empty() {
            Ok(Some(monitor_metrics))
        } else {
            Ok(None)
        }
    }

    fn display(&mut self) -> Result<()> {
        if let Some(m) = self.monitors.get(self.current_idx) {
            let values = map!("{display}" => m.name.clone(),
                              "{brightness}" => m.brightness.to_string(),
                              "{brightness_icon}" => self.shared_config.get_icon("backlight_full").unwrap_or_default().trim().to_string(),
                              "{resolution}" => m.resolution.clone(),
                              "{res_icon}" => self.shared_config.get_icon("resolution").unwrap_or_default().trim().to_string());

            self.text.set_icon("xrandr");
            let format_str = if self.resolution {
                if self.icons {
                    "{display} {brightness_icon} {brightness} {res_icon} {resolution}"
                } else {
                    "{display}: {brightness} [{resolution}]"
                }
            } else if self.icons {
                "{display} {brightness_icon} {brightness}"
            } else {
                "{display}: {brightness}"
            };

            if let Ok(fmt_template) = FormatTemplate::from_string(format_str) {
                self.text.set_text(fmt_template.render_static_str(&values)?);
            }
        }

        Ok(())
    }
}

impl ConfigBlock for Xrandr {
    type Config = XrandrConfig;

    fn new(
        id: usize,
        block_config: Self::Config,
        shared_config: SharedConfig,
        _tx_update_request: Sender<Task>,
    ) -> Result<Self> {
        let mut step_width = block_config.step_width;
        if step_width > 50 {
            step_width = 50;
        }
        Ok(Xrandr {
            text: TextWidget::new(id, 0, shared_config.clone()).with_icon("xrandr"),
            id,
            update_interval: block_config.interval,
            current_idx: 0,
            icons: block_config.icons,
            resolution: block_config.resolution,
            step_width,
            monitors: Vec::new(),
            shared_config,
        })
    }
}

impl Block for Xrandr {
    fn update(&mut self) -> Result<Option<Update>> {
        if let Some(am) = Xrandr::get_active_monitors()? {
            if let Some(mm) = Xrandr::get_monitor_metrics(&am)? {
                self.monitors = mm;
                self.display()?;
            }
        }

        Ok(Some(self.update_interval.into()))
    }

    fn view(&self) -> Vec<&dyn I3BarWidget> {
        vec![&self.text]
    }

    fn click(&mut self, e: &I3BarEvent) -> Result<()> {
        match e.button {
            MouseButton::Left => {
                if self.current_idx < self.monitors.len() - 1 {
                    self.current_idx += 1;
                } else {
                    self.current_idx = 0;
                }
            }
            mb => {
                use LogicalDirection::*;
                match self.shared_config.scrolling.to_logical_direction(mb) {
                    Some(Up) => {
                        if let Some(monitor) = self.monitors.get_mut(self.current_idx) {
                            if monitor.brightness <= (100 - self.step_width) {
                                monitor.set_brightness(self.step_width as i32);
                            }
                        }
                    }
                    Some(Down) => {
                        if let Some(monitor) = self.monitors.get_mut(self.current_idx) {
                            if monitor.brightness >= self.step_width {
                                monitor.set_brightness(-(self.step_width as i32));
                            }
                        }
                    }
                    None => {}
                }
            }
        }
        self.display()?;

        Ok(())
    }

    fn id(&self) -> usize {
        self.id
    }
}
