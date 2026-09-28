import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { useOpencodeFormState } from "@/components/providers/forms/hooks/useOpencodeFormState";

const renderOpencodeFormState = (
  initialSettingsConfig: Record<string, unknown>,
) => {
  let settingsConfig = JSON.stringify(initialSettingsConfig);
  const onSettingsConfigChange = vi.fn((nextConfig: string) => {
    settingsConfig = nextConfig;
  });

  const hook = renderHook(() =>
    useOpencodeFormState({
      appId: "opencode",
      initialData: { settingsConfig: initialSettingsConfig },
      onSettingsConfigChange,
      getSettingsConfig: () => settingsConfig,
    }),
  );

  return {
    ...hook,
    onSettingsConfigChange,
    getSettingsConfig: () => settingsConfig,
  };
};

function renderOpencodeForm(settingsConfig: Record<string, unknown>) {
  let latestConfig = JSON.stringify(settingsConfig, null, 2);
  const onSettingsConfigChange = vi.fn((config: string) => {
    latestConfig = config;
  });
  const getSettingsConfig = vi.fn(() => latestConfig);

  const hook = renderHook(() =>
    useOpencodeFormState({
      initialData: { settingsConfig },
      appId: "opencode",
      providerId: "test-provider",
      onSettingsConfigChange,
      getSettingsConfig,
    }),
  );

  return {
    ...hook,
    onSettingsConfigChange,
    getSettingsConfig,
    readConfig: () => JSON.parse(latestConfig) as Record<string, any>,
  };
}

describe("useOpencodeFormState", () => {
  it("hydrates provider headers from options", () => {
    const { result } = renderOpencodeFormState({
      npm: "@ai-sdk/openai-compatible",
      options: {
        headers: {
          "HTTP-Referer": "https://cc-switch.app",
          "X-Title": "CC Switch",
        },
      },
      models: {},
    });

    expect(result.current.opencodeHeaders).toEqual({
      "HTTP-Referer": "https://cc-switch.app",
      "X-Title": "CC Switch",
    });
  });

  it("writes provider headers to options", () => {
    const { result, getSettingsConfig } = renderOpencodeFormState({
      npm: "@ai-sdk/openai-compatible",
      options: {},
      models: {},
    });

    act(() => {
      result.current.handleOpencodeHeadersChange({
        "X-Title": "CC Switch",
      });
    });

    expect(JSON.parse(getSettingsConfig()).options.headers).toEqual({
      "X-Title": "CC Switch",
    });
  });

  it("removes options.headers when all provider headers are removed", () => {
    const { result, getSettingsConfig } = renderOpencodeFormState({
      npm: "@ai-sdk/openai-compatible",
      options: {
        headers: {
          "X-Title": "CC Switch",
        },
      },
      models: {},
    });

    act(() => {
      result.current.handleOpencodeHeadersChange({});
    });

    expect(JSON.parse(getSettingsConfig()).options.headers).toBeUndefined();
  });

  it("preserves legitimate headers whose names start with header-", () => {
    const { result, getSettingsConfig } = renderOpencodeFormState({
      npm: "@ai-sdk/openai-compatible",
      options: {
        headers: {
          "header-version": "v1",
          "X-Title": "Old",
        },
      },
      models: {},
    });

    act(() => {
      result.current.handleOpencodeHeadersChange({
        "header-version": "v1",
        "X-Title": "New",
      });
    });

    expect(JSON.parse(getSettingsConfig()).options.headers).toEqual({
      "header-version": "v1",
      "X-Title": "New",
    });
  });

  it("preserves legitimate options whose names start with option-", () => {
    const { result, getSettingsConfig } = renderOpencodeFormState({
      npm: "@ai-sdk/openai-compatible",
      options: {
        "option-mode": "legacy",
        timeout: 100,
      },
      models: {},
    });

    act(() => {
      result.current.handleOpencodeExtraOptionsChange({
        "option-mode": "legacy",
        timeout: "200",
        "draft-option:123": "",
      });
    });

    expect(JSON.parse(getSettingsConfig()).options).toEqual({
      "option-mode": "legacy",
      timeout: 200,
    });
  });
});

describe("useOpencodeFormState (auth.json credential store)", () => {
  it("reads legacy options.apiKey without exposing or rewriting it as an extra option", () => {
    const { result, readConfig } = renderOpencodeForm({
      npm: "@ai-sdk/openai-compatible",
      options: {
        baseURL: "https://api.example.com/v1",
        apiKey: "LEGACY_FAKE_KEY",
        setCacheKey: true,
        timeout: 6000,
      },
      models: {},
    });

    expect(result.current.opencodeApiKey).toBe("LEGACY_FAKE_KEY");
    expect(result.current.opencodeExtraOptions).toEqual({
      setCacheKey: "true",
      timeout: "6000",
    });

    act(() => {
      result.current.handleOpencodeExtraOptionsChange({
        ...result.current.opencodeExtraOptions,
        apiKey: '"SHOULD_NOT_RETURN"',
        timeout: "7000",
      });
    });

    const config = readConfig();
    expect(config.options.apiKey).toBeUndefined();
    expect(config.options.setCacheKey).toBe(true);
    expect(config.options.timeout).toBe(7000);
  });

  it("writes non-empty API key edits to auth metadata and removes legacy inline key", () => {
    const { result, readConfig } = renderOpencodeForm({
      npm: "@ai-sdk/openai-compatible",
      auth: {
        source: "opencode_auth_json",
        type: "api",
        key: "OLD_AUTH_FAKE_KEY",
      },
      options: {
        baseURL: "https://api.example.com/v1",
        apiKey: "LEGACY_FAKE_KEY",
      },
      models: {},
    });

    expect(result.current.opencodeApiKey).toBe("OLD_AUTH_FAKE_KEY");

    act(() => {
      result.current.handleOpencodeApiKeyChange("NEW_AUTH_FAKE_KEY");
    });

    const config = readConfig();
    expect(config.auth).toEqual({
      source: "opencode_auth_json",
      type: "api",
      key: "NEW_AUTH_FAKE_KEY",
    });
    expect(config.options.apiKey).toBeUndefined();
    expect(result.current.opencodeApiKey).toBe("NEW_AUTH_FAKE_KEY");
  });

  it("preserves non-API auth objects when clearing the normal API key field", () => {
    const oauthAuth = {
      source: "opencode_auth_json",
      type: "oauth",
      refresh: "FAKE_REFRESH",
      access: "FAKE_ACCESS",
      expires: 1234567890,
      accountId: "acct_FAKE",
      custom: { keep: true },
    };
    const { result, readConfig } = renderOpencodeForm({
      npm: "@ai-sdk/openai-compatible",
      auth: oauthAuth,
      options: {
        baseURL: "https://api.example.com/v1",
        apiKey: "LEGACY_FAKE_KEY",
      },
      models: {},
    });

    expect(result.current.opencodeApiKey).toBe("LEGACY_FAKE_KEY");

    act(() => {
      result.current.handleOpencodeApiKeyChange("");
    });

    const config = readConfig();
    expect(config.auth).toEqual(oauthAuth);
    expect(config.options.apiKey).toBeUndefined();
    expect(result.current.opencodeApiKey).toBe("");
  });
});
