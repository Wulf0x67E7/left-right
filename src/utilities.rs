#[cfg(test)]
#[derive(Debug)]
pub struct CounterAddOp(pub i32);

#[cfg(test)]
impl Absorb<CounterAddOp> for i32 {
    type OpLog = Vec<CounterAddOp>;

    fn log_empty(log: &Vec<CounterAddOp>) -> bool {
        log.is_empty()
    }

    fn log_ops<I: IntoIterator<Item = CounterAddOp>>(pending_log: &mut Vec<CounterAddOp>, ops: I) {
        pending_log.extend(ops);
    }

    fn absorb_first(&mut self, pending_log: &mut Vec<CounterAddOp>, _: &Self) {
        for op in pending_log {
            *self += op.0;
        }
    }

    fn absorb_second(&mut self, partial_log: &mut Vec<CounterAddOp>, _: &Self) {
        for op in partial_log.drain(..) {
            *self += op.0;
        }
    }

    fn drop_first(self: Box<Self>) {}

    fn sync_with(&mut self, first: &Self) {
        *self = *first
    }
}
