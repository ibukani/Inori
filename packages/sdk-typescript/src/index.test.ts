import { strict as assert } from "node:assert";
import { test } from "node:test";
import { dispatch } from "./index.ts";
test("handshake rejects an incompatible major without invoking the extension", async () => {
  let calls = 0;
  const handler = async () => { calls++; return {}; };
  const result = await dispatch({jsonrpc:"2.0",id:"1",method:"initialize",params:{protocol_major:99}}, handler);
  assert.notEqual(result.error, null);
  assert.equal(calls, 0);
});
test("non-text results retain their numeric structure", async () => {
  const result = await dispatch({jsonrpc:"2.0",id:"2",method:"invoke",params:{}}, async () => ({pose:[0.25,0.5,0]}));
  assert.deepEqual(result.result, {pose:[0.25,0.5,0]});
});
