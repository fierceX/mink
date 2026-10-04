//! Fenwick index: append/update O(log n), row lookup O(log n).
#[derive(Clone, Default)]
pub(crate) struct HeightIndex {
    heights: Vec<usize>,
    tree: Vec<usize>,
}
impl HeightIndex {
    pub fn len(&self) -> usize {
        self.heights.len()
    }
    pub fn push(&mut self, height: usize) {
        let index = self.heights.len() + 1;
        let start = index - (index & index.wrapping_neg());
        let covered = self.prefix(index - 1) - self.prefix(start);
        self.heights.push(height);
        self.tree.push(covered + height);
    }
    pub fn set(&mut self, index: usize, height: usize) {
        let old = std::mem::replace(&mut self.heights[index], height);
        let mut pos = index + 1;
        while pos <= self.tree.len() {
            self.tree[pos - 1] = self.tree[pos - 1] - old + height;
            pos += pos & pos.wrapping_neg();
        }
    }
    pub fn prefix(&self, count: usize) -> usize {
        let mut pos = count.min(self.len());
        let mut sum = 0;
        while pos > 0 {
            sum += self.tree[pos - 1];
            pos &= pos - 1;
        }
        sum
    }
    pub fn total(&self) -> usize {
        self.prefix(self.len())
    }
    /// First item containing row, or len when row is past the end.
    pub fn locate(&self, row: usize) -> usize {
        let mut pos = 0usize;
        let mut sum = 0;
        let mut bit = self.len().checked_next_power_of_two().unwrap_or(0);
        while bit > 0 {
            let next = pos + bit;
            if next <= self.len() && sum + self.tree[next - 1] <= row {
                sum += self.tree[next - 1];
                pos = next;
            }
            bit >>= 1;
        }
        pos
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn append_update_and_zero_height_rows_preserve_offsets() {
        let mut index = HeightIndex::default();
        for height in [2, 0, 3, 1] {
            index.push(height);
        }
        assert_eq!(index.total(), 6);
        assert_eq!(
            (0..=6).map(|row| index.locate(row)).collect::<Vec<_>>(),
            [0, 0, 2, 2, 2, 3, 4]
        );
        index.set(0, 0);
        index.set(1, 4);
        assert_eq!(index.total(), 8);
        assert_eq!(index.locate(0), 1);
        assert_eq!(index.locate(4), 2);
    }
}
