// Compile the app's TypeScript for Node's test runner, without bundling or mocking source.
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import ts from "typescript";
export async function resolve(specifier, context, next) {
  try { return await next(specifier, context); }
  catch (error) {
    if (specifier.startsWith(".") && context.parentURL) {
      for (const ext of [".ts", ".tsx"]) {
        try { return await next(specifier + ext, context); } catch {}
      }
    }
    throw error;
  }
}
export async function load(url, context, next) {
  // Vite asset imports are URLs; tests need the same module shape, not image decoding.
  if (url.endsWith(".svg")) return { format: "module", shortCircuit: true, source: `export default ${JSON.stringify(url)};` };
  if (/\.tsx?$/.test(url)) {
    const source = await readFile(fileURLToPath(url), "utf8");
    return { format: "module", shortCircuit: true, source: ts.transpileModule(source, {
      compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext, jsx: ts.JsxEmit.ReactJSX },
      fileName: fileURLToPath(url),
    }).outputText };
  }
  return next(url, context);
}
