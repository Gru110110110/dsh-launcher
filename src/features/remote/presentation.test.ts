import { describe, expect, it } from "vitest";
import {
  formatRemotePassword,
  isErrorCode,
  isValidRemotePassword,
  publicStateCopy,
  qrDataUrl,
} from "./presentation";

describe("remote presentation", () => {
  it("builds an SVG data URL with escaped markup", () => {
    const url = qrDataUrl('<svg viewBox="0 0 1 1"><path d="M0 0"/></svg>');
    expect(url.startsWith("data:image/svg+xml;charset=utf-8,")).toBe(true);
    expect(url).toContain(encodeURIComponent("<svg"));
    expect(url).not.toContain("<svg");
    const encoded = url.slice("data:image/svg+xml;charset=utf-8,".length);
    expect(decodeURIComponent(encoded)).toBe(
      '<svg viewBox="0 0 1 1"><path d="M0 0"/></svg>',
    );
  });

  it("groups long passwords into chunks of four characters", () => {
    expect(formatRemotePassword("abcdefgh")).toBe("abcd efgh");
    expect(formatRemotePassword("abcdefghij")).toBe("abcd efgh ij");
  });

  it("leaves short or empty passwords untouched", () => {
    expect(formatRemotePassword("ab")).toBe("ab");
    expect(formatRemotePassword("abcd")).toBe("abcd");
    expect(formatRemotePassword("")).toBe("");
  });

  it("maps the public tunnel state to status copy", () => {
    expect(publicStateCopy("starting")).toEqual({
      key: "remote.publicStarting",
      tone: "info",
    });
    expect(publicStateCopy("failed")).toEqual({
      key: "remote.publicFailed",
      tone: "error",
    });
    expect(publicStateCopy("running")).toBeNull();
    expect(publicStateCopy("off")).toBeNull();
  });

  it("validates exactly eight ASCII digits for custom passwords", () => {
    expect(isValidRemotePassword("12345678")).toBe(true);
    expect(isValidRemotePassword("00000000")).toBe(true);
    expect(isValidRemotePassword("1234567")).toBe(false);
    expect(isValidRemotePassword("123456789")).toBe(false);
    expect(isValidRemotePassword("1234567a")).toBe(false);
    expect(isValidRemotePassword("1234 5678")).toBe(false);
    expect(isValidRemotePassword("１２３４５６７８")).toBe(false);
    expect(isValidRemotePassword("")).toBe(false);
  });

  it("matches AppError codes on unknown errors", () => {
    expect(
      isErrorCode(
        { code: "remoteDisclaimerRequired" },
        "remoteDisclaimerRequired",
      ),
    ).toBe(true);
    expect(
      isErrorCode({ code: "remoteUnavailable" }, "remoteDisclaimerRequired"),
    ).toBe(false);
    expect(isErrorCode(new Error("boom"), "remoteDisclaimerRequired")).toBe(
      false,
    );
    expect(isErrorCode(null, "remoteDisclaimerRequired")).toBe(false);
    expect(
      isErrorCode("remoteDisclaimerRequired", "remoteDisclaimerRequired"),
    ).toBe(false);
  });
});
