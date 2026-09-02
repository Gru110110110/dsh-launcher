const BASE_WIDTH = 260;
const BASE_HEIGHT = 310;

type Point = { x: number; y: number };
type Size = { width: number; height: number };
type Monitor = { workArea: { position: Point; size: Size } };

export type PetWindowOperations = {
  setSize: (width: number, height: number) => Promise<void>;
  setClickThrough: (enabled: boolean) => Promise<void>;
  hide: () => Promise<void>;
  availableMonitors: () => Promise<Monitor[]>;
  outerSize: () => Promise<Size>;
  outerPosition: () => Promise<Point>;
  primaryMonitor: () => Promise<Monitor | null>;
  setPosition: (x: number, y: number) => Promise<void>;
  show: () => Promise<void>;
};

export function enqueuePetWindowSync(
  previous: Promise<void>,
  task: () => Promise<void>,
  reportError: (error: unknown) => void,
): Promise<void> {
  return previous.then(task, task).catch(reportError);
}

export async function syncPetWindow(
  operations: PetWindowOperations,
  options: {
    active: boolean;
    clickThrough: boolean;
    scale: number;
    initialPosition: Point | null;
    positioned: boolean;
  },
  cancelled: () => boolean,
): Promise<boolean> {
  await operations.setSize(
    Math.round(BASE_WIDTH * options.scale),
    Math.round(BASE_HEIGHT * options.scale),
  );
  if (cancelled()) return false;

  await operations.setClickThrough(options.clickThrough);
  if (cancelled()) return false;

  if (!options.active) {
    await operations.hide();
    return false;
  }

  const monitors = await operations.availableMonitors();
  if (cancelled()) return false;
  const outer = await operations.outerSize();
  if (cancelled()) return false;
  const requested = options.positioned
    ? await operations.outerPosition()
    : options.initialPosition;
  if (cancelled()) return false;

  let x = requested?.x;
  let y = requested?.y;
  let monitor = monitors.find(
    ({ workArea }) =>
      x !== undefined &&
      y !== undefined &&
      x >= workArea.position.x &&
      y >= workArea.position.y &&
      x < workArea.position.x + workArea.size.width &&
      y < workArea.position.y + workArea.size.height,
  );
  if (!monitor) {
    monitor = (await operations.primaryMonitor()) ?? monitors[0];
    if (cancelled()) return false;
  }

  let positionApplied = false;
  if (monitor) {
    const area = monitor.workArea;
    x = Math.min(
      Math.max(
        x ?? area.position.x + area.size.width - outer.width - 24,
        area.position.x,
      ),
      area.position.x + Math.max(0, area.size.width - outer.width),
    );
    y = Math.min(
      Math.max(
        y ?? area.position.y + area.size.height - outer.height - 24,
        area.position.y,
      ),
      area.position.y + Math.max(0, area.size.height - outer.height),
    );
    await operations.setPosition(Math.round(x), Math.round(y));
    if (cancelled()) return false;
    positionApplied = true;
  }

  await operations.show();
  return positionApplied;
}
