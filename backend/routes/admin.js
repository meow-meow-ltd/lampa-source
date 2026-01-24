const fs = require('fs');
const db = require('../database');

module.exports = {
    serve: (req, res, next) => {
        db.get("SELECT value FROM settings WHERE key = 'admin_token'", (err, tokenRow) => {
            const validToken = tokenRow?.value;
            if(req.cookies['auth_token'] === validToken){
                // Serve Admin Page if Authenticated
                 fs.readFile('./src/admin/index.html', 'utf8', (err, data) => {
                 if (err) return next();
                  res.setHeader('Content-Type', 'text/html');
                  res.end(data);
             });
         } else {
             // Start simplified Login Page serve
             res.setHeader('Content-Type', 'text/html');
             res.end(`
                 <!DOCTYPE html>
                 <html>
                 <head>
                     <title>Admin Login</title>
                     <style>
                         body { background: #1b1b1b; color: #fff; font-family: sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; }
                         .login-box { background: #2a2a2a; padding: 2em; border-radius: 8px; box-shadow: 0 4px 15px rgba(0,0,0,0.5); text-align: center; }
                         input { padding: 10px; border-radius: 4px; border: 1px solid #444; background: #333; color: white; width: 200px; margin-bottom: 1em; }
                         button { padding: 10px 20px; border-radius: 4px; border: none; background: #6c5ce7; color: white; cursor: pointer; }
                         button:hover { background: #a29bfe; }
                     </style>
                 </head>
                 <body>
                     <div class="login-box">
                         <h2>Admin Access</h2>
                         <input type="password" id="password" placeholder="Password">
                         <br>
                         <button onclick="login()">Login</button>
                     </div>
                     <script>
                         function login() {
                             fetch('/admin/login', {
                                 method: 'POST',
                                 headers: { 'Content-Type': 'application/json' },
                                 body: JSON.stringify({ password: document.getElementById('password').value })
                             }).then(r => {
                                 if (r.ok) location.reload();
                                 else alert('Wrong password');
                             });
                         }
                     </script>
                 </body>
                 </html>
             `);
         }
    });
}
};
