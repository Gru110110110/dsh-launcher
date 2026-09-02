import type { DesktopUpdateState } from "./generated/bindings";

type CopySpec = {
  key: string;
  values?: Record<string, string | number>;
};

type DesktopUpdateAction = {
  appearance: "default" | "primary";
  disabled: boolean;
  spinning: boolean;
  operation: "check" | "install" | null;
  label: CopySpec;
};

function downloadPercent(
  desktopUpdate: Extract<DesktopUpdateState, { kind: "downloading" }>,
): number | null {
  const total = desktopUpdate.total;
  if (total === null || total <= 0) return null;
  return Math.min(100, Math.floor((desktopUpdate.done * 100) / total));
}

export function shouldShowDesktopUpdateAction(
  desktopUpdate: DesktopUpdateState,
): boolean {
  switch (desktopUpdate.kind) {
    case "available":
    case "preparing":
    case "downloading":
    case "installing":
      return true;
    case "failed":
      return desktopUpdate.version !== null;
    case "idle":
    case "checking":
      return false;
  }
}

export function getDesktopUpdateCompactLabel(
  desktopUpdate: DesktopUpdateState,
): CopySpec | null {
  switch (desktopUpdate.kind) {
    case "available":
      return { key: "action.updateDesktopShort" };
    case "preparing":
      return { key: "action.updateDesktopPercent", values: { percent: 0 } };
    case "downloading":
      return {
        key: "action.updateDesktopPercent",
        values: { percent: downloadPercent(desktopUpdate) ?? 0 },
      };
    case "installing":
      return { key: "action.installingDesktopShort" };
    case "failed":
      return desktopUpdate.version === null
        ? null
        : { key: "action.updateDesktopShort" };
    case "idle":
    case "checking":
      return null;
  }
}

export function getDesktopUpdateDetail(
  desktopUpdate: DesktopUpdateState,
): CopySpec {
  switch (desktopUpdate.kind) {
    case "checking":
      return { key: "update.desktop.checking" };
    case "available":
      return {
        key: "update.desktop.available",
        values: { version: desktopUpdate.version },
      };
    case "preparing":
      return {
        key: "update.desktop.preparing",
        values: { version: desktopUpdate.version },
      };
    case "downloading": {
      const percent = downloadPercent(desktopUpdate);
      return {
        key:
          percent !== null
            ? "update.desktop.downloading"
            : "update.desktop.downloadingUnknown",
        values:
          percent !== null
            ? { version: desktopUpdate.version, percent }
            : { version: desktopUpdate.version },
      };
    }
    case "installing":
      return {
        key: "update.desktop.installing",
        values: { version: desktopUpdate.version },
      };
    case "failed":
      return { key: "update.desktop.failed" };
    case "idle":
      return { key: "settings.desktopVersionDetail" };
  }
}

export function getDesktopUpdateAction(
  desktopUpdate: DesktopUpdateState,
): DesktopUpdateAction {
  switch (desktopUpdate.kind) {
    case "idle":
      return {
        appearance: "default",
        disabled: false,
        spinning: false,
        operation: "check",
        label: { key: "action.checkUpdate" },
      };
    case "checking":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.checkingUpdate" },
      };
    case "available":
      return {
        appearance: "primary",
        disabled: false,
        spinning: false,
        operation: "install",
        label: {
          key: "action.updateDesktopVersion",
          values: { version: desktopUpdate.version },
        },
      };
    case "preparing":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.updatingDesktop" },
      };
    case "downloading": {
      const percent = downloadPercent(desktopUpdate);
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label:
          percent === null
            ? { key: "action.updatingDesktop" }
            : {
                key: "action.updatingDesktopProgress",
                values: { percent },
              },
      };
    }
    case "installing":
      return {
        appearance: "default",
        disabled: true,
        spinning: true,
        operation: null,
        label: { key: "action.installingDesktopShort" },
      };
    case "failed":
      return desktopUpdate.version === null
        ? {
            appearance: "default",
            disabled: false,
            spinning: false,
            operation: "check",
            label: { key: "action.checkUpdate" },
          }
        : {
            appearance: "primary",
            disabled: false,
            spinning: false,
            operation: "install",
            label: {
              key: "action.updateDesktopVersion",
              values: { version: desktopUpdate.version },
            },
          };
  }
}
