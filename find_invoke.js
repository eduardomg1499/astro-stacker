const fs = require('fs');
const content = fs.readFileSync('src/main.js', 'utf8');
const lines = content.split('\n');
console.log("Looking for invoke in main.js:");
for (let i = 0; i < lines.length; i++) {
    if (lines[i].includes('invoke(') || lines[i].includes('invoke (')) {
        console.log(`Line ${i + 1}: ${lines[i].trim()}`);
    }
}
