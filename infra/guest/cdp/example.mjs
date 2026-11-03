// Example: search a page in the visible browser from Node.js (POC-027).
// node /usr/local/lib/elsebot/cdp/example.mjs URL QUERY
import { connect, Uncertain } from "/usr/local/lib/elsebot/cdp/cdp.mjs";

const [url, query] = process.argv.slice(2);
const browser = await connect();
try {
  const page = await browser.page({ create: url });
  await page.fill("input[name=q]", query);
  try {
    await page.click("button[type=submit]");
  } catch (e) {
    if (e instanceof Uncertain) {
      // Do not click again blindly: look at the page first.
      console.error("the click may or may not have happened:", e.message);
    }
    throw e;
  }
  await page.waitFor("document.querySelector('#results')", { timeoutMs: 10000 });
  console.log(JSON.stringify({ title: await page.read("document.title"), results: await page.read("document.querySelector('#results').innerText") }));
} finally {
  await browser.close();
}
