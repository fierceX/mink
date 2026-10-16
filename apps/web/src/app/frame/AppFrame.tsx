/**
 * Three-column shell frame. Layout, drag behaviour and the data attributes come
 * from the DSH `ui-layout` AppFrame; the slot plumbing collapses into plain
 * props here, so the frame owns the grid tracks (sidebar | center | rightbar),
 * the rAF-throttled resize handles and the column solve in columns.ts.
 *
 * The right column is a track, not a box: its occupant draws a fixed-width
 * panel against the frame's right edge at the resolved width, and the track
 * only decides whether the centre makes room for it.
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { CSSProperties, ReactNode } from "react";
import {
  CENTER_MIN,
  RIGHTBAR_DEFAULT_RATIO,
  RIGHTBAR_MAX_RATIO,
  RIGHTBAR_MIN,
  SIDEBAR_AUTO_COLLAPSE,
  SIDEBAR_COLLAPSED,
  SIDEBAR_DEFAULT,
  clampWidth,
  computeColumns,
} from "./columns.ts";
import css from "./AppFrame.module.css";

/** Settle window for the track easing: transitionend, or this as the fallback. */
const TRACK_SETTLE_MS = 600;

export interface AppFrameProps {
  /** Left column occupant; receives the resolved width in px. */
  sidebar: (width: number, collapsed: boolean) => ReactNode;
  /** Center column occupant. */
  center: ReactNode;
  /** Right column occupant; rendered only while the track is open. */
  rightbar?: ((width: number) => ReactNode) | undefined;
  /** Sidebar width preference; 0 closes the column. */
  sidebarWidth: number;
  onSidebarWidthChange: (width: number) => void;
  /** Right panel width preference; 0 closes the track. */
  rightbarWidth: number;
  onRightbarWidthChange: (width: number) => void;
  /** Manual re-expand below the auto-collapse breakpoint. */
  narrowExpanded: boolean;
  /** Close the narrow drawer (the scrim and the sidebar's own toggle both do). */
  onNarrowClose: () => void;
  /** Report whether the frame is drawing the sidebar as a drawer. */
  onNarrowChange?: ((narrow: boolean) => void) | undefined;
}

/**
 * One drag handle: pointer capture plus rAF-throttled dx reports against the
 * drag-start origin.
 */
function DragHandle(props: {
  side: "sidebar" | "rightbar";
  left: number;
  onStart: () => void;
  onDrag: (dx: number) => void;
  onEnd: () => void;
}) {
  const [dragging, setDragging] = useState(false);
  const origin = useRef(0);
  const latest = useRef(0);
  const frame = useRef<number | null>(null);
  const capture = useRef<{ element: HTMLDivElement; id: number } | null>(null);
  const callbacks = useRef({ onStart: props.onStart, onDrag: props.onDrag, onEnd: props.onEnd });
  callbacks.current = { onStart: props.onStart, onDrag: props.onDrag, onEnd: props.onEnd };

  const endDrag = useCallback(() => {
    const active = capture.current;
    if (active === null) return;
    capture.current = null;
    if (frame.current !== null) {
      cancelAnimationFrame(frame.current);
      frame.current = null;
    }
    if (active.element.hasPointerCapture(active.id)) active.element.releasePointerCapture(active.id);
    setDragging(false);
    callbacks.current.onEnd();
  }, []);
  useEffect(() => endDrag, [endDrag]);

  const onPointerDown = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || capture.current !== null) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    capture.current = { element: e.currentTarget, id: e.pointerId };
    origin.current = e.clientX;
    latest.current = e.clientX;
    callbacks.current.onStart();
    setDragging(true);
  }, []);
  const onPointerMove = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    if (capture.current?.id !== e.pointerId) return;
    latest.current = e.clientX;
    frame.current ??= requestAnimationFrame(() => {
      frame.current = null;
      callbacks.current.onDrag(latest.current - origin.current);
    });
  }, []);
  const onPointerUp = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    if (capture.current?.id !== e.pointerId) return;
    callbacks.current.onDrag(e.clientX - origin.current);
    endDrag();
  }, [endDrag]);
  const onPointerCancel = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    if (capture.current?.id === e.pointerId) endDrag();
  }, [endDrag]);

  return (
    <div
      className={css.handle}
      style={{ left: props.left }}
      data-side={props.side}
      data-dragging={dragging || undefined}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerCancel}
      onLostPointerCapture={onPointerCancel}
    />
  );
}

/**
 * Render the shell frame.
 * @param props - column occupants, width preferences and their writers.
 * @returns the frame element tree.
 */
export function AppFrame({
  sidebar,
  center,
  rightbar,
  sidebarWidth,
  onSidebarWidthChange,
  rightbarWidth,
  onRightbarWidthChange,
  narrowExpanded,
  onNarrowClose,
  onNarrowChange,
}: AppFrameProps) {
  const frameRef = useRef<HTMLDivElement | null>(null);
  const [viewport, setViewport] = useState(0);
  const [dragging, setDragging] = useState(false);

  // Track the frame's own box (not the window): rAF-throttled ResizeObserver.
  useLayoutEffect(() => {
    const el = frameRef.current;
    if (el === null) return;
    let raf: number | null = null;
    let disposed = false;
    const measure = () => {
      const width = el.getBoundingClientRect().width;
      if (width > 0) setViewport(width);
    };
    measure();
    const observer = new ResizeObserver(() => {
      if (disposed) return;
      raf ??= requestAnimationFrame(() => {
        raf = null;
        measure();
      });
    });
    observer.observe(el);
    return () => {
      disposed = true;
      observer.disconnect();
      if (raf !== null) cancelAnimationFrame(raf);
    };
  }, []);

  const narrow = viewport > 0 && viewport < SIDEBAR_AUTO_COLLAPSE;
  const sidebarCollapsed = narrow ? !narrowExpanded : sidebarWidth === 0;
  const sidebarPreference = sidebarCollapsed ? 0 : sidebarWidth === 0 ? SIDEBAR_DEFAULT : sidebarWidth;
  const rightbarPreference = rightbarWidth > 0 ? rightbarWidth : viewport * RIGHTBAR_DEFAULT_RATIO;
  const cols = computeColumns(
    viewport,
    sidebarPreference,
    rightbarWidth > 0 ? rightbarPreference : 0,
    SIDEBAR_COLLAPSED,
  );
  const colsRef = useRef(cols);
  colsRef.current = cols;

  // Drag bases are frozen for the whole gesture, so dx deltas do not compound
  // and a concession-clamped width never jumps back to the stored preference.
  const sidebarBase = useRef(0);
  const rightbarBase = useRef(0);

  // Track easing is scoped to a discrete open/close toggle; steady-state
  // viewport updates stay instant (no easing chasing the live window edge).
  const [animating, setAnimating] = useState(0);
  const trackToggle = `${sidebarCollapsed}:${rightbarWidth > 0}`;
  const previousToggle = useRef(trackToggle);
  const previousViewport = useRef(viewport);
  useLayoutEffect(() => {
    const viewportChanged = previousViewport.current !== viewport;
    previousViewport.current = viewport;
    if (previousToggle.current === trackToggle) return;
    previousToggle.current = trackToggle;
    if (viewportChanged) return;
    setAnimating((token) => token + 1);
  }, [trackToggle, viewport]);
  useEffect(() => {
    if (animating === 0) return;
    const frame = frameRef.current;
    if (frame === null) return;
    const settle = () => {
      setAnimating(0);
    };
    const onTransitionEnd = (event: TransitionEvent) => {
      if (event.target === frame && event.propertyName === "grid-template-columns") settle();
    };
    frame.addEventListener("transitionend", onTransitionEnd);
    const timer = setTimeout(settle, TRACK_SETTLE_MS);
    return () => {
      frame.removeEventListener("transitionend", onTransitionEnd);
      clearTimeout(timer);
    };
  }, [animating]);

  const onDragEnd = useCallback(() => {
    setDragging(false);
  }, []);
  const onSidebarStart = useCallback(() => {
    sidebarBase.current = colsRef.current.sidebar;
    setDragging(true);
  }, []);
  const onSidebarDrag = useCallback(
    (dx: number) => {
      onSidebarWidthChange(sidebarBase.current + dx);
    },
    [onSidebarWidthChange],
  );
  const onRightbarStart = useCallback(() => {
    rightbarBase.current = colsRef.current.rightbar;
    setDragging(true);
  }, []);
  const onRightbarDrag = useCallback(
    (dx: number) => {
      onRightbarWidthChange(rightbarBase.current - dx);
    },
    [onRightbarWidthChange],
  );

  // The rendered template lets the grid solve the squeeze natively: the centre
  // declares its protected minimum, the right column bids up to its clamped
  // preference. The JS solve keeps only the discrete decisions and drag bases.
  // Inside the drawer the sidebar draws at its preferred width; the drawer's own
  // box decides how much of it fits.
  useEffect(() => {
    onNarrowChange?.(narrow)
  }, [narrow, onNarrowChange])
  const sidebarWidthForNarrow = cols.sidebar === 0 ? SIDEBAR_DEFAULT : Math.max(cols.sidebar, SIDEBAR_DEFAULT)
  const rightbarMax =
    cols.rightbar === 0 ? 0 : clampWidth(rightbarPreference, RIGHTBAR_MIN, viewport * RIGHTBAR_MAX_RATIO);
  const style = {
    gridTemplateColumns: `${narrow ? 0 : cols.sidebar}px minmax(${
      cols.rightbar === 0 ? 0 : CENTER_MIN
    }px, 1fr) minmax(0px, ${rightbarMax}px)`,
  } as CSSProperties;

  return (
    <div
      ref={frameRef}
      className={css.frame}
      style={style}
      data-narrow={narrow || undefined}
      data-sidebar-open={(narrow && narrowExpanded) || undefined}
      data-sidebar-collapsed={sidebarCollapsed || undefined}
      data-rightbar-collapsed={cols.rightbar === 0 || undefined}
      data-dragging={dragging || undefined}
      data-animating={animating > 0 || undefined}
    >
      <div className={css.sidebarCol}>
        {sidebar(narrow ? sidebarWidthForNarrow : cols.sidebar, sidebarCollapsed)}
      </div>
      <div className={css.centerCol}>{center}</div>
      <div className={css.rightbarCol} data-rightbar-col>
        {/* With no track (a frame too narrow to make room) the occupant still
            draws: the column is zero-width and clips nothing, so the panel hangs
            over the centre from the frame's right edge. On a phone it takes the
            whole frame, because a 360px screen has no room to share (the legacy
            workbench's own ≤767px rule). */}
        {rightbar !== undefined && rightbarWidth > 0
          ? rightbar(cols.rightbar > 0 ? cols.rightbar : narrow ? viewport : Math.round(Math.min(360, viewport * 0.8)))
          : null}
      </div>
      {/* Narrow frames put the sidebar over the content: the scrim closes it. */}
      {narrow && narrowExpanded && (
        <button
          type="button"
          className={css.scrim}
          aria-label="关闭侧栏"
          data-shell-scrim
          onClick={onNarrowClose}
        />
      )}
      <div className={css.overlayLayer} data-shell-overlay />
      {!sidebarCollapsed && (
        <DragHandle
          side="sidebar"
          left={cols.sidebar}
          onStart={onSidebarStart}
          onDrag={onSidebarDrag}
          onEnd={onDragEnd}
        />
      )}
      {rightbarWidth > 0 && cols.rightbar > 0 && (
        <DragHandle
          side="rightbar"
          left={viewport - cols.rightbar}
          onStart={onRightbarStart}
          onDrag={onRightbarDrag}
          onEnd={onDragEnd}
        />
      )}
    </div>
  );
}
