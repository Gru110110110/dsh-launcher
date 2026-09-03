import type { PetState } from "@/platform/generated/bindings";

type PetPlaybackDecision = {
  kind: "repeat" | "switch";
  state: PetState;
};

export class PetPlaybackQueue {
  private currentState: PetState;
  private requestedState: PetState;

  constructor(initialState: PetState) {
    this.currentState = initialState;
    this.requestedState = initialState;
  }

  get displayedState(): PetState {
    return this.currentState;
  }

  request(state: PetState): void {
    this.requestedState = state;
  }

  reset(state: PetState): void {
    this.currentState = state;
    this.requestedState = state;
  }

  completeCycle(): PetPlaybackDecision {
    if (this.requestedState === this.currentState) {
      return { kind: "repeat", state: this.currentState };
    }
    this.currentState = this.requestedState;
    return { kind: "switch", state: this.currentState };
  }
}
