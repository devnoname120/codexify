import {BrowserCompanionError} from "./errors";

/** One bounded pre-Send retry for a DOM-replaced model-picker control.
 * No retries after a submitted message or for unknown/semantic failures.
 */
export async function retryPickerAfterControlTimeout<T>(
  attempt:()=>Promise<T>,
  reacquireAndRetry:()=>Promise<T>,
  signal:AbortSignal,
  sameTarget:()=>boolean,
):Promise<T> {
  try {return await attempt();}
  catch(error) {
    if(signal.aborted || !(error instanceof Error) || error.name!=="TimeoutError")throw error;
    signal.throwIfAborted();
    if(!sameTarget())throw new BrowserCompanionError(
      "ChatGPT changed the pairing page while opening its effort picker",
      "external_interference",
      {cause:error},
    );
    // A retry may open the menu, but must never retry an actual Send.
    return reacquireAndRetry();
  }
}
