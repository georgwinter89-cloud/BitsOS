// Takes COMMON, ITEMS and the checker lenses out of a workflow script (plan/workflows/a1-wave.js) as JSON,
// so the SDK runner builds the same items with the same briefs.
import { readFileSync, writeFileSync } from 'node:fs'

const [src, out] = process.argv.slice(2)
const text = readFileSync(src, 'utf8')
const body = text.slice(text.indexOf('const COMMON'), text.indexOf('async function runItem'))
const data = new Function(body + '\nreturn { COMMON, ITEMS, LENSES: [LENS_HAL, LENS_CORRECT, LENS_FACTS, LENS_TESTS] }')()
writeFileSync(out, JSON.stringify(data, null, 1))
console.log(`${data.ITEMS.length} items -> ${out}`)
