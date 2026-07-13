import { create } from "zustand";
import { commands, OnboardingStore } from "@/lib/utils/tauri";
import { useEffect } from "react";

interface OnboardingState {
  onboardingData: OnboardingStore;
  isLoading: boolean;
  error: string | null;

  // Actions
  loadOnboardingStatus: () => Promise<void>;
  completeOnboarding: (
    afterPersist?: () => Promise<void> | void,
  ) => Promise<void>;
  resetOnboarding: () => Promise<void>;
}

export const useOnboarding = create<OnboardingState>((set, get) => ({
  onboardingData: {
    isCompleted: false,
    completedAt: null,
    currentStep: null,
  },
  isLoading: false,
  error: null,

  loadOnboardingStatus: async () => {
    try {
      set({ isLoading: true, error: null });
      const result = await commands.getOnboardingStatus();

      if (result.status === "ok") {
        set({ onboardingData: result.data, isLoading: false });
      } else {
        throw new Error(result.error);
      }
    } catch (error) {
      console.error("Error loading onboarding status:", error);
      set({
        error:
          error instanceof Error
            ? error.message
            : "Failed to load onboarding status",
        isLoading: false,
      });
    }
  },

  completeOnboarding: async (afterPersist) => {
    try {
      // Keep the current step mounted while the completion mutation runs. The
      // page-level loading state is only for initial status loading; toggling it
      // here used to discard completion errors and reset the ready screen.
      set({ error: null });
      const result = await commands.completeOnboarding();

      if (result.status === "ok") {
        // Run any last work that belongs to the onboarding webview before
        // setting isCompleted. The page reacts to that state by opening Home
        // and closing this window.
        await afterPersist?.();

        // Update local state
        set((state) => ({
          onboardingData: {
            ...state.onboardingData,
            isCompleted: true,
            completedAt: new Date().toISOString(),
          },
        }));
      } else {
        throw new Error(result.error);
      }
    } catch (error) {
      console.error("Error completing onboarding:", error);
      set({
        error:
          error instanceof Error
            ? error.message
            : "Failed to complete onboarding",
      });
      throw error;
    }
  },

  resetOnboarding: async () => {
    try {
      set({ isLoading: true, error: null });
      const result = await commands.resetOnboarding();

      if (result.status === "ok") {
        // Update local state
        set((state) => ({
          onboardingData: {
            ...state.onboardingData,
            isCompleted: false,
            completedAt: null,
            currentStep: null,
          },
          isLoading: false,
        }));
      } else {
        throw new Error(result.error);
      }
    } catch (error) {
      console.error("Error resetting onboarding:", error);
      set({
        error:
          error instanceof Error ? error.message : "Failed to reset onboarding",
        isLoading: false,
      });
      throw error;
    }
  },
}));

// Hook to automatically load onboarding status on mount
export const useOnboardingWithLoader = () => {
  const store = useOnboarding();

  useEffect(() => {
    store.loadOnboardingStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return store;
};
