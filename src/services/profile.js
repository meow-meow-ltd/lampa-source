import ProfileSettings from './profile_settings'

let data = {}

let fetch_promise = null
let fetch_timer = null

function fetch() {
    if (fetch_promise) return fetch_promise

    fetch_promise = new Promise((resolve, reject) => {
        clearTimeout(fetch_timer)
        fetch_timer = setTimeout(() => {
            ProfileSettings.all().then((result) => {
                data = result || {}
                resolve(data)
            }).catch(reject).finally(() => {
                fetch_promise = null
            })
        }, 500)
    })

    return fetch_promise
}

function get(key, default_value) {
    return Object.prototype.hasOwnProperty.call(data, key) ? data[key] : default_value
}

function set(key, value) {
    data[key] = value
    return ProfileSettings.set(key, value)
}

export default {
    fetch,
    get,
    set,
    all: () => data
}
