import { describe, it, expect } from "vitest";

/// Sanity check that the vitest + happy-dom harness boots and DOM globals
/// are present. If this fails the harness is broken; if it passes the
/// other tests in this tree can rely on `document`, timers, etc.
describe("test harness", () => {
  it("provides a DOM", () => {
    expect(typeof document).toBe("object");
    const el = document.createElement("div");
    el.textContent = "hi";
    expect(el.textContent).toBe("hi");
  });

  it("supports fake timers", () => {
    expect(typeof setTimeout).toBe("function");
  });
});
