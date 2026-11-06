export function formatBytes(value: number | null | undefined): string {
  if (value === null || value === undefined) return "-";
  if (value === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const index = Math.min(units.length - 1, Math.floor(Math.log(Math.abs(value)) / Math.log(1024)));
  const scaled = value / Math.pow(1024, index);
  const digits = scaled >= 100 || index === 0 ? 0 : 1;
  return `${scaled.toFixed(digits)} ${units[index]}`;
}

export function formatDateTime(ts: number | null | undefined): string {
  if (!ts) return "-";
  return new Date(ts).toLocaleString(undefined, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatTime(ts: number | null | undefined): string {
  if (!ts) return "-";
  return new Date(ts).toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function formatDate(ts: number | null | undefined): string {
  if (!ts) return "-";
  return new Date(ts).toLocaleDateString();
}

export function relativeTime(ts: number | null | undefined): string {
  if (!ts) return "-";
  const diff = Date.now() - ts;
  const minutes = Math.floor(diff / 60000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} d ago`;
  const months = Math.floor(days / 30);
  if (months < 12) return `${months} mo ago`;
  return `${Math.floor(months / 12)} y ago`;
}

export function eventLabel(event: string): string {
  switch (event) {
    case "created":
      return "Created";
    case "modified":
      return "Modified";
    case "renamed":
      return "Renamed";
    case "deleted":
      return "Deleted";
    case "restored":
      return "Restored";
    case "pre-restore":
      return "Before restore";
    case "snapshot":
      return "Snapshot";
    case "baseline":
      return "Baseline";
    default:
      return event;
  }
}

export function fileName(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}

export function fileExtension(path: string): string {
  const name = fileName(path);
  const index = name.lastIndexOf(".");
  if (index <= 0) return "";
  return name.slice(index + 1).toLowerCase();
}
