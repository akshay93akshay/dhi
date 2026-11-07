"use client";

import { useEffect, useId, useRef, useState } from "react";
import Link from "next/link";

import { cn } from "@/lib/utils";
import { serviceStatusText, type ServiceState } from "@/lib/ui";
import styles from "./dhi-brand-status.module.css";

export type BrandActivity =
  | "idle"
  | "focus-goal"
  | "focus-planner"
  | "focus-worker"
  | "focus-submit"
  | "typing"
  | "submit"
  | "success"
  | "error"
  | "wink";

export type DhiBrandStatusProps = {
  state: ServiceState;
  /** Face-only tile for the collapsed sidebar and mobile top bar. */
  compact?: boolean;
  /** Show the corner status badge on a compact tile. */
  badge?: boolean;
  /** Larger hero treatment for the Overview start page. */
  hero?: boolean;
  /** Task-detail lockup: wider element, same brand mark. */
  detail?: boolean;
  /** Pupils follow the pointer while the mark is hovered. */
  interactive?: boolean;
  /** Controlled expression/gesture layered over the service state. */
  activity?: BrandActivity;
  className?: string;
};

type Gaze = { x: number; y: number };

const CENTER: Gaze = { x: 0, y: 0 };

function activityGaze(activity: BrandActivity): Gaze {
  switch (activity) {
    case "focus-goal":
      return { x: 0, y: 4 };
    case "focus-planner":
      return { x: -4.5, y: 0 };
    case "focus-worker":
      return { x: 4.5, y: 0 };
    case "focus-submit":
      return { x: 0, y: 4 };
    case "submit":
      return { x: 4, y: 0 };
    default:
      return CENTER;
  }
}

/**
 * Centered brand mark for the sidebar header. The status lives in the
 * separate sidebar footer so the logo stays a clean, centered brand block.
 */
export function DhiBrandStatus({
  state,
  compact = false,
  badge = false,
  hero = false,
  detail = false,
  interactive = false,
  activity = "idle",
  className,
}: DhiBrandStatusProps) {
  const clipId = `dhi-face-${useId().replaceAll(":", "")}`;
  const [gaze, setGaze] = useState<Gaze>(() => activityGaze(activity));
  const [localWink, setLocalWink] = useState(false);
  const lastPointerRef = useRef(0);
  const winkTimerRef = useRef<number | null>(null);
  const effectiveActivity: BrandActivity = localWink ? "wink" : activity;
  const label = `DHI Plus — Dynamic Heuristic Intelligence Plus. ${serviceStatusText(state)}.`;

  // Clear a pending click-wink timer if the logo unmounts mid-gesture.
  useEffect(
    () => () => {
      if (winkTimerRef.current !== null) {
        window.clearTimeout(winkTimerRef.current);
      }
    },
    [],
  );

  useEffect(() => {
    setGaze(activityGaze(effectiveActivity));
  }, [effectiveActivity, state]);

  // Autonomous idle scan. Pointer activity suppresses it briefly. Error keeps
  // the eyes centred; reduced motion disables the scan entirely. Controlled
  // focus/typing gestures take over while an activity is active.
  useEffect(() => {
    if (!interactive || effectiveActivity !== "idle") {
      return;
    }
    if (state === "error") {
      setGaze(CENTER);
      return;
    }
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      return;
    }
    const positions =
      state === "loading" ? [3, -3, 1, 0] : [0, 2, -2, 0];
    let step = 0;
    const timer = window.setInterval(() => {
      if (Date.now() - lastPointerRef.current < 2_200) {
        return;
      }
      setGaze({ x: positions[step % positions.length], y: 0 });
      step += 1;
    }, 5_200);
    return () => window.clearInterval(timer);
  }, [effectiveActivity, interactive, state]);

  function handlePointerMove(
    event: React.PointerEvent<HTMLAnchorElement>,
  ): void {
    if (!interactive) {
      return;
    }
    const rect = event.currentTarget.getBoundingClientRect();
    if (rect.width === 0) {
      return;
    }
    lastPointerRef.current = Date.now();
    const relative = (event.clientX - rect.left) / rect.width;
    const normalized = Math.max(-1, Math.min(1, (relative - 0.235) / 0.13));
    setGaze({ x: normalized * 3.6, y: 0 });
  }

  function handlePointerLeave(): void {
    if (!interactive) {
      return;
    }
    lastPointerRef.current = Date.now();
    setGaze(activityGaze(effectiveActivity));
  }

  function handleKeyDown(event: React.KeyboardEvent<HTMLAnchorElement>): void {
    if (!interactive) {
      return;
    }
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      lastPointerRef.current = Date.now();
      setGaze((current) => ({
        x: Math.max(-7, current.x - 3),
        y: current.y,
      }));
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      lastPointerRef.current = Date.now();
      setGaze((current) => ({
        x: Math.min(7, current.x + 3),
        y: current.y,
      }));
    } else if (event.key.toLowerCase() === "c") {
      setGaze(activityGaze(effectiveActivity));
    }
  }

  function handleClick(event: React.MouseEvent<HTMLAnchorElement>): void {
    if (!interactive) {
      return;
    }
    event.preventDefault();
    setLocalWink(true);
    if (winkTimerRef.current !== null) {
      window.clearTimeout(winkTimerRef.current);
    }
    winkTimerRef.current = window.setTimeout(() => {
      setLocalWink(false);
      winkTimerRef.current = null;
    }, 700);
  }

  if (compact) {
    return (
      <Link
        href="/"
        aria-label={label}
        title={label}
        className={cn(styles.markLink, className)}
        onClick={handleClick}
      >
        <span className={styles.markTileCompact}>
          <DhiMark
            state={state}
            clipId={clipId}
            gaze={CENTER}
            activity={effectiveActivity}
          />
          {badge ? (
            <span
              className={styles.badge}
              data-state={state}
              aria-hidden="true"
            />
          ) : null}
        </span>
      </Link>
    );
  }

  return (
    <Link
      href="/"
      aria-label={label}
      title={label}
      tabIndex={interactive ? 0 : undefined}
      className={cn(
        styles.block,
        hero && styles.hero,
        detail && styles.detail,
        className,
      )}
      onPointerMove={interactive ? handlePointerMove : undefined}
      onPointerLeave={interactive ? handlePointerLeave : undefined}
      onKeyDown={interactive ? handleKeyDown : undefined}
      onClick={handleClick}
    >
      <DhiMark
        state={state}
        clipId={clipId}
        gaze={gaze}
        activity={effectiveActivity}
        full
      />
    </Link>
  );
}

/** Sidebar footer status. The logo does not repeat the status text. */
export function DhiServiceStatus({
  state,
  collapsed = false,
}: {
  state: ServiceState;
  collapsed?: boolean;
}) {
  const text = serviceStatusText(state);

  if (collapsed) {
    return (
      <div className={styles.statusCompact} title={text}>
        <span className={styles.statusDot} data-state={state} aria-hidden="true" />
        <span className="sr-only">{text}</span>
      </div>
    );
  }

  return (
    <div
      className={styles.statusBar}
      data-state={state}
      aria-live="polite"
    >
      <span className={styles.statusDot} data-state={state} aria-hidden="true" />
      {text}
    </div>
  );
}

function DhiMark({
  state,
  clipId,
  gaze,
  activity,
  full = false,
}: {
  state: ServiceState;
  clipId: string;
  gaze: Gaze;
  activity: BrandActivity;
  full?: boolean;
}) {
  // Controlled success/error gestures override the service expression; the
  // service state still drives the status dot and the resting face.
  const expression: ServiceState =
    activity === "success" ? "healthy" : activity === "error" ? "error" : state;

  return (
    <svg
      className={full ? styles.logoFull : styles.mark}
      viewBox={full ? "0 0 700 270" : "55 25 220 190"}
      role="img"
      aria-hidden="true"
      data-state={state}
      data-activity={activity}
      preserveAspectRatio="xMidYMid meet"
    >
      <defs>
        <clipPath id={clipId}>
          <path d="M105 83 H178 C213 83 235 105 235 137 C235 169 213 191 178 191 H105 Z" />
        </clipPath>
      </defs>

      <path className={styles.antennaStem} d="M166 48 V75" />
      <circle className={styles.antennaBase} cx="166" cy="79" r="5" />
      <circle className={styles.antennaLight} cx="166" cy="43" r="10" />

      <path
        className={styles.dShell}
        fillRule="evenodd"
        d="
          M70 72 H174
          C226 72 260 98 260 137
          C260 176 226 202 174 202
          H70 Z
          M104 96 H172
          C208 96 230 110 230 137
          C230 164 208 178 172 178
          H104 Z"
      />

      <rect
        className={styles.sidePort}
        x="246"
        y="126"
        width="11"
        height="22"
        rx="5.5"
      />

      <g clipPath={`url(#${clipId})`}>
        <path
          className={cn(styles.brow, styles.browLeft)}
          d={
            expression === "healthy"
              ? "M124 118 Q139 105 153 116"
              : expression === "error"
                ? "M124 120 Q139 101 153 108"
                : "M124 111 Q139 96 153 111"
          }
        />
        <path
          className={cn(styles.brow, styles.browRight)}
          d={
            expression === "healthy"
              ? "M169 116 Q184 105 199 118"
              : expression === "error"
                ? "M169 108 Q184 101 199 120"
                : "M169 118 Q184 108 199 113"
          }
        />

        <circle
          className={cn(styles.eyeWhite, styles.eyeWhiteLeft)}
          cx="140"
          cy="143"
          r="17"
        />
        <circle
          className={cn(styles.eyeWhite, styles.eyeWhiteRight)}
          cx="184"
          cy="143"
          r="17"
        />
        <circle
          className={cn(styles.lens, styles.lensEye, styles.lensLeft)}
          cx="140"
          cy="143"
          r="17"
        />
        <circle
          className={cn(styles.lens, styles.lensEye, styles.lensRight)}
          cx="184"
          cy="143"
          r="17"
        />
        <path className={cn(styles.lens, styles.lensBridge)} d="M157 143 H167" />

        <g
          className={cn(styles.pupilGroup, styles.pupilGroupLeft)}
          style={{ transform: `translate(${gaze.x}px, ${gaze.y}px)` }}
        >
          <circle className={styles.pupil} cx="140" cy="143" r="7" />
          <circle className={styles.pupilCore} cx="142" cy="141" r="2" />
        </g>
        <g
          className={cn(styles.pupilGroup, styles.pupilGroupRight)}
          style={{ transform: `translate(${gaze.x}px, ${gaze.y}px)` }}
        >
          <circle className={styles.pupil} cx="184" cy="143" r="7" />
          <circle className={styles.pupilCore} cx="186" cy="141" r="2" />
        </g>

        {/* Happy closed-eye arcs for the success gesture. */}
        <path
          className={styles.happyEye}
          d="M124 145 Q140 130 156 145"
        />
        <path
          className={styles.happyEye}
          d="M168 145 Q184 130 200 145"
        />

        {/* Left-eye wink line; the right eye stays open. */}
        <path className={styles.winkLine} d="M124 144 Q140 137 156 144" />

        <ellipse
          className={cn(styles.blinkLid, styles.blinkLidLeft)}
          cx="140"
          cy="143"
          rx="18"
          ry="17"
        />
        <ellipse
          className={cn(styles.blinkLid, styles.blinkLidRight)}
          cx="184"
          cy="143"
          rx="18"
          ry="17"
        />

        <path
          className={cn(styles.sadLid, styles.sadLidLeft)}
          d="M123 141 Q140 119 157 141 L157 130 Q140 115 123 130 Z"
        />
        <path
          className={cn(styles.sadLid, styles.sadLidRight)}
          d="M167 141 Q184 119 201 141 L201 130 Q184 115 167 130 Z"
        />

        <path
          className={styles.mouth}
          d={
            expression === "healthy"
              ? "M145 164 Q162 187 179 164"
              : expression === "error"
                ? "M145 175 Q162 153 179 175"
                : "M156 166 Q162 160 168 166 Q169 176 162 178 Q155 176 156 166 Z"
          }
        />
      </g>

      {full ? (
        <>
          <text className={styles.wordmark} x="286" y="187">
            H
          </text>
          <text className={styles.wordmark} x="410" y="187">
            I
          </text>
          <text
            className={styles.wordmark}
            x="452"
            y="125"
            style={{ fontSize: 48, letterSpacing: 0 }}
          >
            +
          </text>
          <text className={styles.tagline} x="350" y="237" textAnchor="middle">
            DYNAMIC HEURISTIC INTELLIGENCE
            <tspan className={styles.taglinePlus} dy="-6">
              +
            </tspan>
          </text>
        </>
      ) : null}
    </svg>
  );
}
