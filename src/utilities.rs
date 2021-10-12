#[cfg(test)]
#[derive(Debug, Default)]
pub struct CounterAddOps(pub Vec<i32>);

#[cfg(test)]
impl Absorb<CounterAddOps> for i32 {
    fn is_empty(ops: &CounterAddOps) -> bool {
        ops.0.is_empty()
    }

    fn absorb_first(&mut self, pending_ops: &mut CounterAddOps, _: &Self) {
        for op in pending_ops.0.iter_mut() {
            *self += *op;
        }
    }

    fn absorb_second(&mut self, partial_ops: &mut CounterAddOps, _: &Self) {
        for op in partial_ops.0.drain(..) {
            *self += op;
        }
    }

    fn drop_first(self: Box<Self>) {}
}
