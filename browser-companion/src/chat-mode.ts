import type { Page } from "patchright";
import { BrowserCompanionError } from "./errors";

// Codexify only supports Chat. Opt its dedicated browser out of the Work UI
// rollout; the merged model picker can otherwise select Work even in projects
// which cannot use it. Account/workspace access controls are left intact.
// Keep this function self-contained: Chrome runs it before page scripts.
export function installChatModeCompatibility(): void {
  if (location.origin !== "https://chatgpt.com" || window !== window.top) return;
  type Properties = Record<string, any>;
  const root = window as unknown as Properties;
  if (root.__codexifyChatModeCompatibility) return;
  root.__codexifyChatModeCompatibility = true;
  const clients = new WeakSet<object>();
  const registries = new WeakSet<object>();
  const wrappers = new WeakMap<Function, Function>();
  const isObject = (value: unknown): value is Properties => value !== null && typeof value === "object";

  function observe(owner: Properties, key: string, transform: (value: any) => any): void {
    const descriptor = Object.getOwnPropertyDescriptor(owner, key);
    if (descriptor && (!descriptor.configurable || descriptor.get || descriptor.set)) return;
    let value = transform(owner[key]);
    Object.defineProperty(owner, key, {
      configurable:true, enumerable:descriptor?.enumerable ?? true,
      get:() => value, set:next => { value = transform(next); },
    });
  }

  function wrapGetFeatureGate(original: any): any {
    if (typeof original !== "function") return original;
    const existing = wrappers.get(original);
    if (existing) return existing;
    const wrapped = function(this: unknown, name: string, ...args: unknown[]) {
      const gate = Reflect.apply(original, this, [name, ...args]) as Properties | undefined;
      return name === "1625797156" && gate ? {...gate, value:false} : gate;
    };
    wrappers.set(original, wrapped);
    wrappers.set(wrapped, wrapped);
    return wrapped;
  }

  function client(value: any): any {
    if (!isObject(value) || clients.has(value)) return value;
    clients.add(value);
    // Statsig registers the instance from its base constructor, before the
    // subclass assigns getFeatureGate. Observe that assignment and replacements.
    observe(value, "getFeatureGate", wrapGetFeatureGate);
    return value;
  }

  function registry(value: any): any {
    if (!isObject(value) || registries.has(value)) return value;
    registries.add(value);
    observe(value, "firstInstance", client);
    observe(value, "instances", instances => {
      if (!isObject(instances)) return instances;
      Object.values(instances).forEach(client);
      return new Proxy(instances, {
        set(target, key, next) { return Reflect.set(target, key, client(next)); },
      });
    });
    return value;
  }

  observe(root, "__STATSIG__", registry);
  // Restored tabs can already have initialized before the companion attaches.
  const current = root.__STATSIG__?.firstInstance;
  current?.$emt?.({name:"values_updated", status:current.loadingStatus, values:current.getContext?.().values});
}

export async function selectChatMode(page: Page, signal?: AbortSignal): Promise<void> {
  const chat = page.locator('button[aria-pressed]').filter({hasText:/^Chat$/}).filter({visible:true});
  if (await chat.count() !== 1) return; // Older layouts and existing conversations have no switch.
  if (await chat.getAttribute("aria-pressed") === "true") return;
  await chat.click({timeout:5_000, signal});
  const deadline = Date.now() + 5_000;
  do {
    signal?.throwIfAborted();
    if (await chat.getAttribute("aria-pressed").catch(() => null) === "true") return;
    await new Promise(resolve => setTimeout(resolve, 50));
  } while (Date.now() < deadline);
  throw new BrowserCompanionError("ChatGPT did not switch to Chat mode; nothing was sent", "selector_drift");
}
