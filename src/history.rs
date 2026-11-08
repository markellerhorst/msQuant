//! Bounded undo/redo history.
pub struct History<T> { undo: Vec<T>, redo: Vec<T>, max: usize }

impl<T> History<T> {
    /// Creates an empty history that keeps at most `max` steps in each direction.
    pub fn new(max: usize) -> Self { History { undo: Vec::new(), redo: Vec::new(), max } }
    /// Record the state BEFORE a change. Clears the redo stack.
    pub fn record(&mut self, state: T) {
        self.undo.push(state);
        if self.undo.len() > self.max { self.undo.remove(0); }
        self.redo.clear();
    }
    /// Whether there is a step to undo.
    pub fn can_undo(&self) -> bool { !self.undo.is_empty() }
    /// Whether there is a step to redo.
    pub fn can_redo(&self) -> bool { !self.redo.is_empty() }
    /// The step that [`History::undo`] would return, without removing it.
    pub fn peek_undo(&self) -> Option<&T> { self.undo.last() }
    /// The step that [`History::redo`] would return, without removing it.
    pub fn peek_redo(&self) -> Option<&T> { self.redo.last() }
    /// Go one step back; `current` is stored so it can be redone.
    pub fn undo(&mut self, current: T) -> Option<T> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        if self.redo.len() > self.max { self.redo.remove(0); }
        Some(prev)
    }
    /// Goes one step forward; `current` is stored so that the step can be undone again.
    pub fn redo(&mut self, current: T) -> Option<T> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        if self.undo.len() > self.max { self.undo.remove(0); }
        Some(next)
    }
    /// Forgets all steps.
    pub fn clear(&mut self) { self.undo.clear(); self.redo.clear(); }
    /// Number of undo and redo steps, in this order.
    pub fn lens(&self) -> (usize, usize) { (self.undo.len(), self.redo.len()) }
}
