import { describe, expect, it } from 'vitest';
import { readingGesture } from './readingGesture';
describe('mobile reading gesture',()=>{
  it('requires sustained movement and ignores subpixel jitter',()=>{
    const gesture=readingGesture();expect(gesture.consume(1)).toBeUndefined();expect(gesture.consume(20)).toBeUndefined();expect(gesture.consume(27)).toBeUndefined();expect(gesture.consume(2)).toBe(true);
  });
  it('starts a new threshold when the scroll direction reverses',()=>{
    const gesture=readingGesture();gesture.consume(40);expect(gesture.consume(-10)).toBeUndefined();expect(gesture.consume(-38)).toBe(false);
  });
  it('reset prevents another touch or session from inheriting a partial gesture',()=>{
    const gesture=readingGesture();gesture.consume(40);gesture.reset();expect(gesture.consume(20)).toBeUndefined();expect(gesture.consume(28)).toBe(true);
  });
  it('ignores invalid deltas and restarts accumulation after a transition',()=>{
    const gesture=readingGesture();expect(gesture.consume(NaN)).toBeUndefined();expect(gesture.consume(Infinity)).toBeUndefined();expect(gesture.consume(60)).toBe(true);expect(gesture.consume(10)).toBeUndefined();expect(gesture.consume(-48)).toBe(false);
  });
});
