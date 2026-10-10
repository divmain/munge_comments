// Größe: identifiers and strings with multi-byte characters.
const größe = "日本語"; // after CJK text
const emoji = "🎉🚀"; /* after emoji */
const café = { naïve: "ü" }; // 🎉 an emoji inside the comment
/* Ελληνικά in a block comment */
export const all = [größe, emoji, café];
