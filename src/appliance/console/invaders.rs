//! The console's invaders game: one invader for each instance running on
//! this node, labelled with the instance it stands for (#2150's easter egg).
//!
//! A hit pops the invader and nothing else, unless the cluster has turned
//! on kill mode with `[appliance] console_chaos = true` (#2151). Then a hit
//! also asks bun to kill that instance, as the ordinary Smoker fault
//! `relish fault kill` sends, and bun decides: it checks the cluster's
//! setting itself, so nothing typed on this keyboard can turn kill mode
//! on. This module is only the game: where things are and what a key
//! does. The event loop sends the kill; the view draws the field.

use std::collections::BTreeSet;

use crate::bun::top::TopRow;

/// The playing field's width, the Linux console's 80 columns less the
/// tab's borders. A larger screen shows the same field.
pub const FIELD_WIDTH: usize = 78;

/// Invaders in a row.
pub const PER_ROW: usize = 5;

/// How many rows fit on the Linux console's 25 lines. Invaders beyond
/// them wait their turn, off the field.
pub const MAX_ROWS: usize = 3;

/// The columns each invader owns: its sprite and its label, with a gap.
pub const CELL_WIDTH: usize = 12;

/// How far one key press moves the cannon.
const CANNON_STEP: usize = 2;

/// One invader: the instance it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invader {
    pub app: String,
    pub namespace: String,
    pub instance: String,
}

/// What the last shot did, for the line under the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shot {
    Missed,
    /// Harmless: the invader is gone, the instance isn't touched.
    Popped {
        instance: String,
    },
    /// Kill mode: the kill is on its way to bun.
    Sending {
        instance: String,
    },
    /// Kill mode: Smoker killed the instance, as fault `fault_id`.
    Killed {
        instance: String,
        fault_id: u64,
    },
    /// Kill mode: bun or Smoker said no; the invader comes back.
    Refused {
        instance: String,
        reason: String,
    },
}

/// The game's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    /// The cannon's column on the field.
    pub cannon: usize,
    /// How far right the formation has marched.
    pub march: usize,
    /// Which way it marches next.
    pub marching_left: bool,
    /// The instances popped this wave, which stay off the field until the
    /// next one, even though the workload list still names them.
    pub popped: BTreeSet<String>,
    /// The column of the last shot's laser, drawn until the next tick.
    pub laser: Option<usize>,
    pub last: Option<Shot>,
    /// A kill for the event loop to send, taken with [`Game::take_kill`].
    kill: Option<Invader>,
}

impl Default for Game {
    fn default() -> Self {
        Self {
            cannon: FIELD_WIDTH / 2,
            march: 0,
            marching_left: false,
            popped: BTreeSet::new(),
            laser: None,
            last: None,
            kill: None,
        }
    }
}

/// The invaders for `rows`, in the workload list's order: one per app
/// instance, leaving out the ones popped this wave. A job row stands for
/// all of a job's tasks on the node, not one container, so it gets none.
pub fn invaders(rows: &[TopRow], popped: &BTreeSet<String>) -> Vec<Invader> {
    rows.iter()
        .filter(|row| row.tasks.is_none() && !popped.contains(&row.instance.id))
        .map(|row| Invader {
            app: row.instance.app_name.clone(),
            namespace: row.instance.namespace.clone(),
            instance: row.instance.id.clone(),
        })
        .collect()
}

/// How far a formation of `count` invaders can march across the field.
fn slack(count: usize) -> usize {
    let columns = count.clamp(1, PER_ROW);
    FIELD_WIDTH.saturating_sub(columns * CELL_WIDTH - 1)
}

impl Game {
    /// Where invader `index` stands: its row, and the first column of its
    /// cell. `None` for one waiting off the field.
    pub fn position(&self, index: usize) -> Option<(usize, usize)> {
        let row = index / PER_ROW;
        (row < MAX_ROWS).then(|| (row, self.march + (index % PER_ROW) * CELL_WIDTH))
    }

    /// A second passed: the formation of `count` invaders marches one
    /// column, turning at the field's edges, and the laser fades.
    pub fn tick(&mut self, count: usize) {
        self.laser = None;
        let slack = slack(count);
        if self.march >= slack {
            self.march = slack;
            self.marching_left = true;
        } else if self.march == 0 {
            self.marching_left = false;
        }
        if slack == 0 {
            return;
        }
        if self.marching_left {
            self.march -= 1;
        } else {
            self.march += 1;
        }
    }

    /// Move the cannon left (`-1`) or right (`1`), stopping at the edges.
    pub fn move_cannon(&mut self, direction: isize) {
        let step = CANNON_STEP as isize * direction.signum();
        self.cannon = self.cannon.saturating_add_signed(step).min(FIELD_WIDTH - 1);
    }

    /// Fire straight up. The lowest invader over the cannon is hit: it
    /// pops, and in kill mode its instance is queued to be killed.
    pub fn fire(&mut self, invaders: &[Invader], kill_mode: bool) {
        self.laser = Some(self.cannon);
        let hit = (0..invaders.len())
            .filter_map(|index| Some((index, self.position(index)?)))
            .filter(|(_, (_, column))| (*column..*column + CELL_WIDTH - 1).contains(&self.cannon))
            .max_by_key(|(_, (row, _))| *row)
            .map(|(index, _)| &invaders[index]);
        let Some(invader) = hit else {
            self.last = Some(Shot::Missed);
            return;
        };
        let instance = invader.instance.clone();
        self.popped.insert(instance.clone());
        self.last = Some(if kill_mode {
            self.kill = Some(invader.clone());
            Shot::Sending { instance }
        } else {
            Shot::Popped { instance }
        });
    }

    /// The kill the last shot queued, if any, for the event loop to send.
    pub fn take_kill(&mut self) -> Option<Invader> {
        self.kill.take()
    }

    /// What bun said to a kill: the fault's id, or why it refused. A
    /// refused invader comes back, since its instance still runs.
    pub fn kill_answered(&mut self, instance: String, answer: Result<u64, String>) {
        self.last = Some(match answer {
            Ok(fault_id) => Shot::Killed { instance, fault_id },
            Err(reason) => {
                self.popped.remove(&instance);
                Shot::Refused { instance, reason }
            }
        });
    }

    /// Bring every invader back.
    pub fn new_wave(&mut self) {
        self.popped.clear();
        self.last = None;
        self.laser = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invader(instance: &str) -> Invader {
        Invader {
            app: instance.split('-').next().unwrap_or(instance).into(),
            namespace: "default".into(),
            instance: instance.into(),
        }
    }

    fn formation(count: usize) -> Vec<Invader> {
        (0..count)
            .map(|index| invader(&format!("web-{index}")))
            .collect()
    }

    /// A game with the formation at the left edge and the cannon under
    /// invader `index`'s column.
    fn aimed_at(index: usize) -> Game {
        Game {
            cannon: (index % PER_ROW) * CELL_WIDTH + 3,
            ..Game::default()
        }
    }

    #[test]
    fn a_hit_pops_the_invader_and_nothing_else_by_default() {
        let invaders = formation(3);
        let mut game = aimed_at(1);
        game.fire(&invaders, false);
        assert_eq!(
            game.last,
            Some(Shot::Popped {
                instance: "web-1".into()
            })
        );
        assert!(game.popped.contains("web-1"));
        assert_eq!(game.take_kill(), None, "no kill without kill mode");
        assert_eq!(game.laser, Some(game.cannon));
    }

    #[test]
    fn in_kill_mode_a_hit_queues_a_kill_of_its_instance_once() {
        let invaders = formation(3);
        let mut game = aimed_at(2);
        game.fire(&invaders, true);
        assert_eq!(
            game.last,
            Some(Shot::Sending {
                instance: "web-2".into()
            })
        );
        assert_eq!(game.take_kill(), Some(invader("web-2")));
        assert_eq!(game.take_kill(), None);
    }

    #[test]
    fn a_shot_between_invaders_misses() {
        let invaders = formation(3);
        let mut game = Game {
            cannon: CELL_WIDTH - 1,
            ..Game::default()
        };
        game.fire(&invaders, true);
        assert_eq!(game.last, Some(Shot::Missed));
        assert!(game.popped.is_empty());
        assert_eq!(game.take_kill(), None);
    }

    #[test]
    fn the_lowest_invader_over_the_cannon_is_hit_first() {
        let invaders = formation(PER_ROW + 1);
        let mut game = aimed_at(0);
        game.fire(&invaders, false);
        let hit = format!("web-{PER_ROW}");
        assert_eq!(game.last, Some(Shot::Popped { instance: hit }));
    }

    #[test]
    fn invaders_off_the_field_cant_be_hit() {
        let game = Game::default();
        assert_eq!(
            game.position(PER_ROW * MAX_ROWS - 1).map(|(row, _)| row),
            Some(2)
        );
        assert_eq!(game.position(PER_ROW * MAX_ROWS), None);
    }

    #[test]
    fn a_refused_kill_brings_its_invader_back() {
        let invaders = formation(1);
        let mut game = aimed_at(0);
        game.fire(&invaders, true);
        game.kill_answered("web-0".into(), Err("kill mode is off".into()));
        assert!(game.popped.is_empty());
        assert!(matches!(game.last, Some(Shot::Refused { .. })));
        game.fire(&invaders, true);
        game.kill_answered("web-0".into(), Ok(7));
        assert!(game.popped.contains("web-0"));
        assert_eq!(
            game.last,
            Some(Shot::Killed {
                instance: "web-0".into(),
                fault_id: 7
            })
        );
    }

    #[test]
    fn the_formation_marches_to_the_edge_and_back() {
        let mut game = Game::default();
        let slack = slack(PER_ROW);
        for _ in 0..slack {
            game.tick(PER_ROW);
        }
        assert_eq!(game.march, slack);
        game.tick(PER_ROW);
        assert_eq!(game.march, slack - 1, "it turns at the right edge");
        for _ in 0..slack + 3 {
            game.tick(PER_ROW);
        }
        assert!(game.march <= slack);
    }

    #[test]
    fn the_cannon_stops_at_the_edges() {
        let mut game = Game::default();
        for _ in 0..FIELD_WIDTH {
            game.move_cannon(-1);
        }
        assert_eq!(game.cannon, 0);
        for _ in 0..FIELD_WIDTH {
            game.move_cannon(1);
        }
        assert_eq!(game.cannon, FIELD_WIDTH - 1);
    }

    #[test]
    fn a_new_wave_brings_every_invader_back() {
        let invaders = formation(2);
        let mut game = aimed_at(0);
        game.fire(&invaders, false);
        game.new_wave();
        assert!(game.popped.is_empty());
        assert_eq!(game.last, None);
    }

    #[test]
    fn job_rows_and_popped_instances_get_no_invader() {
        use crate::bun::agent::InstanceStatus;
        let row = |id: &str, tasks: Option<u64>| TopRow {
            node: "node-1".into(),
            instance: InstanceStatus {
                id: id.into(),
                app_name: id.split('-').next().unwrap_or(id).into(),
                namespace: "default".into(),
                state: "running".into(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: None,
                image: None,
                data_on_node: false,
                reason: None,
                sidecars: Vec::new(),
            },
            cpu_percent: None,
            memory_bytes: None,
            tasks,
        };
        let rows = vec![
            row("web-0", None),
            row("nightly", Some(4)),
            row("web-1", None),
        ];
        let popped = BTreeSet::from(["web-1".to_string()]);
        assert_eq!(invaders(&rows, &popped), vec![invader("web-0")]);
    }
}
