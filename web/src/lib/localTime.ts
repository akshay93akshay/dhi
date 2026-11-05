/**
 * User-facing time helpers.
 *
 * Storage and API values stay in UTC. These helpers convert them to the
 * browser/system timezone only when displaying them.
 */

function pad(value: number): string {
  return String(value).padStart(2, "0");
}

function parsed(iso: string): Date | null {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? null : date;
}

function zoneLabel(date: Date): string {
  try {
    const part = new Intl.DateTimeFormat(undefined, {
      timeZoneName: "short",
    })
      .formatToParts(date)
      .find((item) => item.type === "timeZoneName");
    return part?.value ?? "local";
  } catch {
    return "local";
  }
}

/** `YYYY-MM-DD HH:MM:SS <system zone>` in the system timezone. */
export function formatLocalTimestamp(iso: string): string {
  const date = parsed(iso);
  if (date === null) {
    return iso;
  }
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    ` ${formatLocalClock(iso)} ${zoneLabel(date)}`
  );
}

/** `HH:MM:SS` in the system timezone. */
export function formatLocalClock(iso: string): string {
  const date = parsed(iso);
  if (date === null) {
    return iso;
  }
  return (
    `${pad(date.getHours())}:${pad(date.getMinutes())}:` +
    `${pad(date.getSeconds())}`
  );
}

/** `YYYY-MM-DD` in the system timezone. */
export function formatLocalDate(iso: string): string {
  const date = parsed(iso);
  if (date === null) {
    return iso;
  }
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}
