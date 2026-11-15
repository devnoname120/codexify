import {expect,test} from "bun:test";
import {browserSimplicityCss,browserSimplicityFromEnvironment,parseBrowserSimplicity,MINIMAL_CHATGPT_CSS,BONES_CHATGPT_CSS} from "../src/minimal-page";

test("browser simplicity validates all levels and keeps legacy environment behavior",()=>{
  const savedLevel=process.env.CODEXIFY_BROWSER_SIMPLICITY,savedLegacy=process.env.CODEXIFY_BROWSER_MINIMAL_UI;
  try {
    delete process.env.CODEXIFY_BROWSER_SIMPLICITY;
    delete process.env.CODEXIFY_BROWSER_MINIMAL_UI;
    expect(browserSimplicityFromEnvironment()).toBe("off");
    process.env.CODEXIFY_BROWSER_MINIMAL_UI="1";
    expect(browserSimplicityFromEnvironment()).toBe("simplified");
    for(const level of ["off","simplified","bones"] as const){
      process.env.CODEXIFY_BROWSER_SIMPLICITY=level;
      expect(browserSimplicityFromEnvironment()).toBe(level);
      expect(parseBrowserSimplicity(level)).toBe(level);
    }
    for(const invalid of ["fast","",true,null,1])expect(()=>parseBrowserSimplicity(invalid)).toThrow("off, simplified, or bones");
  }finally{
    if(savedLevel===undefined)delete process.env.CODEXIFY_BROWSER_SIMPLICITY;else process.env.CODEXIFY_BROWSER_SIMPLICITY=savedLevel;
    if(savedLegacy===undefined)delete process.env.CODEXIFY_BROWSER_MINIMAL_UI;else process.env.CODEXIFY_BROWSER_MINIMAL_UI=savedLegacy;
  }
});

test("Off has no CSS and Bones includes the shared history optimization",()=>{
  expect(browserSimplicityCss("off")).toBe("");
  expect(browserSimplicityCss("simplified")).toBe(MINIMAL_CHATGPT_CSS);
  expect(browserSimplicityCss("bones")).toBe(BONES_CHATGPT_CSS);
  expect(BONES_CHATGPT_CSS.startsWith(MINIMAL_CHATGPT_CSS)).toBeTrue();
});
