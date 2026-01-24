const bodyParser = require('body-parser');
const cookieParser = require('cookie-parser');
const authParams = require('./routes/auth');
const settingsParams = require('./routes/settings');
const adminParams = require('./routes/admin');
const dataParams = require('./routes/data');
const profilesParams = require('./routes/profiles');
const bookmarksParams = require('./routes/bookmarks');
const timelineParams = require('./routes/timeline');
const backupParams = require('./routes/backup');

module.exports = function(req, res, next) {
    cookieParser()(req, res, () => {
        bodyParser.urlencoded({ extended: true })(req, res, () => {
            bodyParser.json()(req, res, () => {
                if (req.url === '/admin/login' && req.method === 'POST') {
                    authParams.login(req, res);
                } else if (req.url === '/admin/logout' && req.method === 'POST') {
                    authParams.logout(req, res);
                } else if (req.url === '/admin') {
                    adminParams.serve(req, res, next);
                } else if (req.url.startsWith('/api/settings')) {
                    settingsParams.handle(req, res);
                } else if (req.url.startsWith('/api/value')) {
                    dataParams.handle(req, res);
                } else if (req.url.startsWith('/api/profiles')) {
                    profilesParams.handle(req, res);
                } else if (req.url.startsWith('/api/bookmarks')) {
                    bookmarksParams.handle(req, res);
                } else if (req.url.startsWith('/api/timeline')) {
                    timelineParams.handle(req, res);
                } else if (req.url.startsWith('/api/users/backup')) {
                    backupParams.handle(req, res);
                } else {
                    next();
                }
            });
        });
    });
};
