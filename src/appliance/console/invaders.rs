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
    /// The instance id, `{namespace}__{app}-{n}`, which the kill names.
    pub instance: String,
    /// What the field shows under the sprite: the id without its namespace,
    /// unless another invader on the field would then read the same.
    pub label: String,
    /// The instance's restart count when the list was read, so the game can
    /// tell a killed instance that restarted from one that never died.
    pub restarts: u32,
}

/// What bun said to a kill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillAnswer {
    /// Smoker killed it, as this fault.
    Killed { fault_id: u64 },
    /// Bun or Smoker said no, and why: the instance wasn't touched.
    Refused { reason: String },
    /// No clear answer (a timeout, a dropped connection, a server error):
    /// the kill may still land.
    Unknown { reason: String },
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
    /// Kill mode: no clear answer, so the game holds fire until the
    /// workload list shows whether the instance died.
    Unknown {
        instance: String,
        reason: String,
    },
    /// Kill mode: after an unclear answer, the workload list shows the
    /// instance died.
    Gone {
        instance: String,
    },
}

/// How long the game holds fire after an unclear answer when the workload
/// list never shows the instance leave `running`. By then bun's own window
/// for counting a recent kill (`smoker::types::KILL_SETTLE`) has passed
/// too, so a kill that landed late can't have been missed by the rail.
pub const UNCONFIRMED_HOLD_SECONDS: u64 = 30;

/// The kill the game is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Awaiting {
    instance: String,
    restarts: u32,
    /// When bun's answer came back unclear; `None` while there's none yet.
    unconfirmed_since: Option<u64>,
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
    /// The kill bun hasn't clearly answered yet. Kill mode holds fire until
    /// it has, or the workload list shows how it went, so the screen never
    /// says a kill failed while it may still land.
    awaiting: Option<Awaiting>,
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
            awaiting: None,
        }
    }
}

/// The invaders for `rows`, in the workload list's order: one per running
/// app instance, leaving out the ones popped this wave. A job gets none: a
/// task-array row stands for all of a job's tasks on the node, and a
/// singleton run's `run-N` row isn't an instance bun's kill can name. An
/// instance that isn't `running` has nothing to kill yet.
pub fn invaders(rows: &[TopRow], popped: &BTreeSet<String>) -> Vec<Invader> {
    let mut invaders: Vec<Invader> = rows
        .iter()
        .filter(|row| is_app_instance(row) && !popped.contains(&row.instance.id))
        .map(|row| Invader {
            app: row.instance.app_name.clone(),
            namespace: row.instance.namespace.clone(),
            instance: row.instance.id.clone(),
            label: short_label(&row.instance.id, &row.instance.namespace).to_string(),
            restarts: row.instance.restart_count,
        })
        .collect();
    let shown = invaders.len().min(PER_ROW * MAX_ROWS);
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for invader in &invaders[..shown] {
        *seen.entry(invader.label.clone()).or_default() += 1;
    }
    for invader in &mut invaders[..shown] {
        if seen.get(&invader.label).is_some_and(|count| *count > 1) {
            invader.label = format!("{}/{}", invader.namespace, invader.label);
        }
    }
    invaders
}

/// Whether a workload row is a running app instance, the only kind of row
/// that gets an invader.
fn is_app_instance(row: &TopRow) -> bool {
    row.tasks.is_none() && !row.instance.id.starts_with("run-") && row.instance.state == "running"
}

/// An instance id without its `{namespace}__` prefix: `web-0` for
/// `default__web-0`.
fn short_label<'a>(id: &'a str, namespace: &str) -> &'a str {
    id.strip_prefix(namespace)
        .and_then(|rest| rest.strip_prefix("__"))
        .unwrap_or(id)
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
        if kill_mode && self.awaiting.is_some() {
            return;
        }
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
            self.awaiting = Some(Awaiting {
                instance: instance.clone(),
                restarts: invader.restarts,
                unconfirmed_since: None,
            });
            Shot::Sending { instance }
        } else {
            Shot::Popped { instance }
        });
    }

    /// The kill the last shot queued, if any, for the event loop to send.
    pub fn take_kill(&mut self) -> Option<Invader> {
        self.kill.take()
    }

    /// What bun said to a kill, at `now`. A refused invader comes back,
    /// since its instance still runs. After an unclear answer the game
    /// keeps holding fire: see [`Game::workloads_seen`].
    pub fn kill_answered(&mut self, instance: String, answer: KillAnswer, now: u64) {
        let awaiting = self
            .awaiting
            .take()
            .filter(|awaiting| awaiting.instance == instance);
        self.last = Some(match answer {
            KillAnswer::Killed { fault_id } => Shot::Killed { instance, fault_id },
            KillAnswer::Refused { reason } => {
                self.popped.remove(&instance);
                Shot::Refused { instance, reason }
            }
            KillAnswer::Unknown { reason } => {
                self.awaiting = awaiting.map(|awaiting| Awaiting {
                    unconfirmed_since: Some(now),
                    ..awaiting
                });
                Shot::Unknown { instance, reason }
            }
        });
    }

    /// The workload list arrived at `now`. After an unclear answer, it
    /// settles the kill: the instance has left `running` or restarted, so
    /// the kill landed; or it's been [`UNCONFIRMED_HOLD_SECONDS`], so the
    /// game stops waiting.
    pub fn workloads_seen(&mut self, rows: &[TopRow], now: u64) {
        let Some(awaiting) = &self.awaiting else {
            return;
        };
        let Some(since) = awaiting.unconfirmed_since else {
            return;
        };
        let still_running = rows.iter().any(|row| {
            row.instance.id == awaiting.instance
                && row.instance.state == "running"
                && row.instance.restart_count == awaiting.restarts
        });
        if !still_running {
            self.last = Some(Shot::Gone {
                instance: awaiting.instance.clone(),
            });
            self.awaiting = None;
        } else if now.saturating_sub(since) >= UNCONFIRMED_HOLD_SECONDS {
            self.awaiting = None;
        }
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

    /// The invader for instance `label` (`web-0`) in `default`, with the
    /// id bun gives it.
    fn invader(label: &str) -> Invader {
        Invader {
            app: label.split('-').next().unwrap_or(label).into(),
            namespace: "default".into(),
            instance: format!("default__{label}"),
            label: label.into(),
            restarts: 0,
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
                instance: "default__web-1".into()
            })
        );
        assert!(game.popped.contains("default__web-1"));
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
                instance: "default__web-2".into()
            })
        );
        assert_eq!(game.take_kill(), Some(invader("web-2")));
        assert_eq!(game.take_kill(), None);
    }

    #[test]
    fn kill_mode_holds_fire_until_bun_answers_the_last_kill() {
        let invaders = formation(2);
        let mut game = aimed_at(0);
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), Some(invader("web-0")));
        game.cannon = CELL_WIDTH + 3;
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), None, "a second kill while one is out");
        assert!(!game.popped.contains("default__web-1"));
        game.kill_answered(
            "default__web-0".into(),
            KillAnswer::Killed { fault_id: 1 },
            0,
        );
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), Some(invader("web-1")));
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
        let hit = format!("default__web-{PER_ROW}");
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
        game.kill_answered(
            "default__web-0".into(),
            KillAnswer::Refused {
                reason: "kill mode is off".into(),
            },
            0,
        );
        assert!(game.popped.is_empty());
        assert!(matches!(game.last, Some(Shot::Refused { .. })));
        game.fire(&invaders, true);
        game.kill_answered(
            "default__web-0".into(),
            KillAnswer::Killed { fault_id: 7 },
            0,
        );
        assert!(game.popped.contains("default__web-0"));
        assert_eq!(
            game.last,
            Some(Shot::Killed {
                instance: "default__web-0".into(),
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

    /// A workload row as `/v1/top` gives it.
    fn row(id: &str, app: &str, namespace: &str, state: &str, tasks: Option<u64>) -> TopRow {
        TopRow {
            node: "node-1".into(),
            instance: crate::bun::agent::InstanceStatus {
                id: id.into(),
                app_name: app.into(),
                namespace: namespace.into(),
                state: state.into(),
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
        }
    }

    /// Only running app instances get invaders: not a task-array row
    /// (`add_job_rows`'s `tasks`), not a singleton job's `run-N` row, not an
    /// instance that's still starting, and not one popped this wave.
    #[test]
    fn only_running_app_instances_get_an_invader() {
        let rows = vec![
            row("default__web-0", "web", "default", "running", None),
            row("tasks", "nightly", "default", "running", Some(4)),
            row("run-42", "migrate", "default", "running", None),
            row("default__web-1", "web", "default", "running", None),
            row("default__web-2", "web", "default", "pending", None),
        ];
        let popped = BTreeSet::from(["default__web-1".to_string()]);
        assert_eq!(invaders(&rows, &popped), vec![invader("web-0")]);
    }

    /// After an unclear answer the game holds fire until the list shows the
    /// instance died (or restarted), and never says it survived.
    #[test]
    fn an_unclear_answer_holds_fire_until_the_list_shows_the_kill() {
        let invaders = formation(2);
        let mut game = aimed_at(0);
        game.fire(&invaders, true);
        assert!(game.take_kill().is_some());
        game.kill_answered(
            "default__web-0".into(),
            KillAnswer::Unknown {
                reason: "no answer within 10 s".into(),
            },
            100,
        );
        assert!(matches!(game.last, Some(Shot::Unknown { .. })));
        assert!(
            game.popped.contains("default__web-0"),
            "not shown as survived"
        );

        let running = |restarts| {
            let mut row = row("default__web-0", "web", "default", "running", None);
            row.instance.restart_count = restarts;
            row
        };
        game.workloads_seen(&[running(0)], 105);
        game.cannon = CELL_WIDTH + 3;
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), None, "still holding fire");

        game.workloads_seen(&[running(1)], 110);
        assert_eq!(
            game.last,
            Some(Shot::Gone {
                instance: "default__web-0".into()
            })
        );
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), Some(invader("web-1")));
    }

    #[test]
    fn an_unclear_answer_stops_holding_fire_after_a_while() {
        let invaders = formation(2);
        let mut game = aimed_at(0);
        game.fire(&invaders, true);
        assert!(game.take_kill().is_some());
        game.kill_answered(
            "default__web-0".into(),
            KillAnswer::Unknown {
                reason: "502".into(),
            },
            100,
        );
        let still = [row("default__web-0", "web", "default", "running", None)];
        game.workloads_seen(&still, 100 + UNCONFIRMED_HOLD_SECONDS - 1);
        game.cannon = CELL_WIDTH + 3;
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), None);
        game.workloads_seen(&still, 100 + UNCONFIRMED_HOLD_SECONDS);
        game.fire(&invaders, true);
        assert_eq!(game.take_kill(), Some(invader("web-1")));
    }

    /// A label drops the namespace, and keeps it only where two invaders on
    /// the field would otherwise read the same.
    #[test]
    fn labels_drop_the_namespace_unless_two_would_collide() {
        let rows = vec![
            row("default__web-0", "web", "default", "running", None),
            row("team-b__web-0", "web", "team-b", "running", None),
            row("team-b__api-g2-1", "api", "team-b", "running", None),
        ];
        let labels: Vec<String> = invaders(&rows, &BTreeSet::new())
            .into_iter()
            .map(|invader| invader.label)
            .collect();
        assert_eq!(labels, ["default/web-0", "team-b/web-0", "api-g2-1"]);
    }
}
