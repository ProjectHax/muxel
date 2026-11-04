//! Worker-owned aggregation of terminal intervals with no input or slow work.

#[derive(Default)]
pub(super) struct IdleTicks {
    pub ticks: u64,
    pub window_ms: u64,
    pub writes: u64,
    pub write_bytes: u64,
    pub write_us: u64,
    pub write_max_us: u64,
    pub write_queue_us: u64,
    pub write_queue_max_us: u64,
    pub reads: u64,
    pub read_bytes: u64,
    pub drain_us: u64,
    pub drain_max_us: u64,
    pub drain_n: u64,
    pub queue_bytes: u64,
    pub queue_chunks: u64,
    pub notify: u64,
    pub batches: u64,
    pub process_bytes: u64,
    pub process_us: u64,
    pub process_max_us: u64,
    pub paints: u64,
    pub paint_us: u64,
    pub paint_max_us: u64,
    pub paint_focused: u64,
    pub paint_full: u64,
    pub paint_replay: u64,
    pub build_us: u64,
    pub shape_us: u64,
    pub submit_us: u64,
    pub runs: u64,
    pub reused: u64,
}

impl IdleTicks {
    pub fn add(&mut self, next: Self) {
        macro_rules! sum { ($($field:ident),* $(,)?) => { $(self.$field += next.$field;)* }; }
        macro_rules! max { ($($field:ident),* $(,)?) => { $(self.$field = self.$field.max(next.$field);)* }; }
        sum!(
            ticks,
            window_ms,
            writes,
            write_bytes,
            write_us,
            write_queue_us,
            reads,
            read_bytes,
            drain_us,
            drain_n,
            notify,
            batches,
            process_bytes,
            process_us,
            paints,
            paint_us,
            paint_focused,
            paint_full,
            paint_replay,
            build_us,
            shape_us,
            submit_us,
            runs,
            reused
        );
        max!(
            write_max_us,
            write_queue_max_us,
            drain_max_us,
            queue_bytes,
            queue_chunks,
            process_max_us,
            paint_max_us
        );
    }

    pub fn take_line(&mut self) -> Option<String> {
        if self.ticks == 0 {
            return None;
        }
        let s = std::mem::take(self);
        Some(format!(
            "term-prof[v9 idle] Δ={}ms ticks={} keys=0 writer={}/{}B queue_sum={}µs queue_max={}µs write_sum={}µs write_max={}µs reader={}/{}B drain_sum={}µs drain_max={}µs drain_n={} queue_high={}B/{}chunks notify={} process={} batches/{}B sum={}µs max={}µs paint={} sum={}µs max={}µs focus={} full={} replay={} build_sum={}µs shape_sum={}µs submit_sum={}µs runs={} reuse={}",
            s.window_ms,
            s.ticks,
            s.writes,
            s.write_bytes,
            s.write_queue_us,
            s.write_queue_max_us,
            s.write_us,
            s.write_max_us,
            s.reads,
            s.read_bytes,
            s.drain_us,
            s.drain_max_us,
            s.drain_n,
            s.queue_bytes,
            s.queue_chunks,
            s.notify,
            s.batches,
            s.process_bytes,
            s.process_us,
            s.process_max_us,
            s.paints,
            s.paint_us,
            s.paint_max_us,
            s.paint_focused,
            s.paint_full,
            s.paint_replay,
            s.build_us,
            s.shape_us,
            s.submit_us,
            s.runs,
            s.reused,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::IdleTicks;

    #[test]
    fn idle_summary_keeps_totals_and_worst_tick_then_resets() {
        let mut idle = IdleTicks::default();
        for (paint, drain, queue) in [(100, 700, 5), (400, 200, 2), (50, 900, 3)] {
            idle.add(IdleTicks {
                ticks: 1,
                window_ms: 500,
                reads: 2,
                read_bytes: 100,
                paints: 3,
                paint_us: paint,
                paint_max_us: paint,
                drain_us: drain,
                drain_max_us: drain,
                queue_chunks: queue,
                write_max_us: 120,
                write_queue_max_us: 200,
                ..IdleTicks::default()
            });
        }
        let line = idle.take_line().unwrap();
        for field in [
            "Δ=1500ms ticks=3 keys=0",
            "reader=6/300B",
            "drain_sum=1800µs drain_max=900µs",
            "queue_high=0B/5chunks",
            "paint=9 sum=550µs max=400µs",
            "write_max=120µs",
            "queue_max=200µs",
        ] {
            assert!(line.contains(field), "{field}: {line}");
        }
        assert!(idle.take_line().is_none());
    }
}
