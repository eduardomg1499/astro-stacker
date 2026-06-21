const { exec } = require('child_process');
const fs = require('fs');

exec('cargo check', (error, stdout, stderr) => {
    fs.writeFileSync('errors.txt', stderr || '');
    if (stdout) fs.appendFileSync('errors.txt', stdout);
    if (error) fs.appendFileSync('errors.txt', error.toString());
    console.log('done');
});
