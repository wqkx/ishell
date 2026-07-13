//! 网络速率历史。

/// 网络速率历史（用于折线图）。
#[derive(Default)]
pub struct NetHistory {
    pub down: std::collections::VecDeque<f64>,
    pub up: std::collections::VecDeque<f64>,
}

impl NetHistory {
    pub const CAP: usize = 120;
    pub fn push(&mut self, down: f64, up: f64) {
        self.down.push_back(down);
        self.up.push_back(up);
        while self.down.len() > Self::CAP {
            self.down.pop_front();
        }
        while self.up.len() > Self::CAP {
            self.up.pop_front();
        }
    }
    pub fn down_slice(&self) -> Vec<f64> {
        self.down.iter().cloned().collect()
    }
    pub fn up_slice(&self) -> Vec<f64> {
        self.up.iter().cloned().collect()
    }
}
